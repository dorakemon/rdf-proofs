use super::constants::CRYPTOSUITE_PROOF;
use crate::{
    ark_to_base64url,
    blind_signature::{blind_verify, BlindSignRequest, BlindSignRequestString},
    common::{
        canonicalize_graph, generate_proof_spec_context, get_delimiter, get_graph_from_ntriples,
        get_hasher, get_term_from_string, get_vc_from_ntriples, hash_byte_to_field,
        hash_term_to_field, is_nym, multibase_to_ark, randomize_bnodes,
        randomize_bnodes_in_vc_pairs, read_private_var_list, read_public_var_list,
        reorder_vc_triples, BBSPlusDefaultFieldHasher, BBSPlusHash, BBSPlusPublicKey,
        BBSPlusSignature, Fr, PedersenCommitmentStmt, PoKBBSPlusStmt, PoKBBSPlusWit, Proof,
        ProofWithIndexMap, R1CSCircomWitness, StatementIndexMap, Statements,
    },
    constants::PPID_PREFIX,
    context::{
        AUTHENTICATION, CHALLENGE, CIRCUIT, CREATED, CRYPTOSUITE, DATA_INTEGRITY_PROOF, DOMAIN,
        ENCRYPTED_UID, HOLDER, MULTIBASE, PREDICATE, PREDICATE_TYPE, PRIVATE, PROOF, PROOF_PURPOSE,
        PROOF_VALUE, PUBLIC, SECRET_COMMITMENT, VERIFIABLE_CREDENTIAL, VERIFIABLE_CREDENTIAL_TYPE,
        VERIFIABLE_PRESENTATION_TYPE, VERIFICATION_METHOD,
    },
    elliptic_elgamal_verifiable_encryption_with_bbs_plus,
    error::RDFProofsError,
    key_gen::{generate_params, generate_ppid, PPID},
    key_graph::KeyGraph,
    ordered_triple::{
        OrderedGraphViews, OrderedNamedOrBlankNode, OrderedVerifiableCredentialGraphViews,
    },
    predicate::{Circuit, CircuitString},
    signature::verify,
    vc::{
        DisclosedVerifiableCredential, VcPair, VcPairString, VerifiableCredential,
        VerifiableCredentialTriples, VerifiablePresentation,
    },
    ElGamalCiphertext, ElGamalPublicKey, ElGamalVerifiableEncryption,
};
use ark_std::rand::RngCore;
use chrono::offset::Utc;
use multibase::Base;
use oxrdf::{
    vocab::{rdf::TYPE, xsd},
    BlankNode, Dataset, Graph, GraphNameRef, LiteralRef, NamedNode, NamedOrBlankNode, Quad,
    QuadRef, Subject, Term, TermRef, Triple,
};
use proof_system::{
    prelude::{EqualWitnesses, MetaStatements},
    proof_spec::ProofSpec,
    statement::r1cs_legogroth16::R1CSCircomProver,
    witness::{Witness, Witnesses},
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// derive VP from VCs, disclosed VCs, and deanonymization map
pub fn derive_proof<R: RngCore>(
    rng: &mut R,
    vc_pairs: &Vec<VcPair>,
    deanon_map: &HashMap<NamedOrBlankNode, Term>,
    key_graph: &KeyGraph,
    challenge: Option<&str>,
    domain: Option<&str>,
    secret: Option<&[u8]>,
    blind_sign_request: Option<BlindSignRequest>,
    with_ppid: Option<bool>,
    predicates: Vec<Graph>,
    circuits: HashMap<NamedNode, Circuit>,
    opener_pub_key: Option<ElGamalPublicKey>,
) -> Result<Dataset, RDFProofsError> {
    for vc in vc_pairs {
        println!("{}", vc.to_string());
    }
    println!("deanon map:\n{:#?}\n", deanon_map);

    // either VCs or a blind sign request must be provided as input
    if vc_pairs.is_empty() && blind_sign_request.is_none() {
        return Err(RDFProofsError::MissingInputToDeriveProof);
    }

    // TODO:
    // check: each disclosed VCs must be the derived subset of corresponding VCs via deanon map

    // get issuer public keys
    let public_keys = vc_pairs
        .iter()
        .map(|VcPair { original: vc, .. }| get_public_keys(&vc.proof, key_graph))
        .collect::<Result<Vec<_>, _>>()?;
    println!("public keys:\n{:#?}\n", public_keys);

    // verify VCs
    vc_pairs
        .iter()
        .map(
            |VcPair { original: vc, .. }| match (vc.is_bound(), secret) {
                (Ok(false), _) => verify(vc, key_graph),
                (Ok(true), Some(s)) => blind_verify(s, vc, key_graph),
                (Ok(true), None) => Err(RDFProofsError::MissingSecret),
                _ => Err(RDFProofsError::VCWithUnsupportedCryptosuite),
            },
        )
        .collect::<Result<(), _>>()?;

    // randomize blank node identifiers in VC documents and VC proofs
    // for avoiding identifier collisions among multiple VCs
    let randomized_vc_pairs = vc_pairs
        .iter()
        .map(
            |VcPair {
                 original,
                 disclosed,
             }| {
                let (r_original_document, r_disclosed_document) =
                    randomize_bnodes_in_vc_pairs(&original.document, &disclosed.document);
                let (r_original_proof, r_disclosed_proof) =
                    randomize_bnodes_in_vc_pairs(&original.proof, &disclosed.proof);
                VcPair::new(
                    VerifiableCredential::new(r_original_document, r_original_proof),
                    VerifiableCredential::new(r_disclosed_document, r_disclosed_proof),
                )
            },
        )
        .collect::<Vec<_>>();
    for vc in &randomized_vc_pairs {
        println!("randomized vc: {}", vc.to_string());
    }

    // randomize blank node identifiers in predicate graphs
    // except for user-defined blank node identifiers in `deanon_map`
    let anon_bnodes: HashSet<_> = deanon_map.keys().cloned().collect();
    let randomized_predicates = predicates
        .iter()
        .map(|predicate| randomize_bnodes(predicate, &anon_bnodes))
        .collect::<Vec<_>>();

    // split VC pairs into original VCs and disclosed VCs
    let (original_vcs, disclosed_vcs): (Vec<_>, Vec<_>) = randomized_vc_pairs
        .into_iter()
        .map(
            |VcPair {
                 original,
                 disclosed,
             }| (original, disclosed),
        )
        .unzip();

    // get PPID
    let ppid = get_ppid(&domain, &secret, with_ppid)?;

    // encrypt secret as usk
    let verifiable_encryption_for_uid = match (secret, opener_pub_key) {
        (Some(secret), Some(opener_pub_key)) => {
            get_encrypted_secret_and_pok(&opener_pub_key, secret, rng).map(Some)
        }
        (Some(_), None) | (None, None) => Ok(None),
        _ => Err(RDFProofsError::MissingSecretOrOpenerPubKey), // This already returns Err
    }
    .unwrap();
    let cipher_text = verifiable_encryption_for_uid
        .as_ref()
        .map(|e| e.cipher_text)
        .or(None);

    // build VP draft (= canonicalized VP without proofValue) based on disclosed VCs
    let (vp_draft, vp_draft_bnode_map, vc_document_graph_names) = build_vp(
        disclosed_vcs,
        &challenge,
        &domain,
        &blind_sign_request,
        &ppid,
        &cipher_text,
        randomized_predicates,
    )?;

    // decompose VP draft into graphs
    let VerifiablePresentation {
        metadata: _vp_metadata_graph,
        proof: vp_proof_graph,
        proof_graph_name: vp_proof_graph_name,
        disclosed_vcs: canonicalized_disclosed_vc_graphs,
        predicates: predicate_graphs,
    } = (&vp_draft).try_into()?;

    // extract `proofValue`s from original VCs
    let (original_vcs_without_proof_value, vc_proof_values): (Vec<_>, Vec<_>) = original_vcs
        .iter()
        .map(|original_vc| {
            let proof_config = original_vc.get_proof_config();
            let proof_value = original_vc.get_proof_value()?;
            Ok((
                VerifiableCredential::new(original_vc.document.clone(), proof_config),
                proof_value,
            ))
        })
        .collect::<Result<Vec<_>, RDFProofsError>>()?
        .into_iter()
        .unzip();

    // canonicalize original VCs
    let (canonicalized_original_vcs, original_vcs_bnode_map) =
        canonicalize_vcs(&original_vcs_without_proof_value)?;

    for v in &canonicalized_original_vcs {
        println!("canonicalized_original_vcs: {}", v);
    }
    println!("original vcs bnode map: {:#?}", original_vcs_bnode_map);

    // construct extended deanonymization map
    let extended_deanon_map =
        extend_deanon_map(deanon_map, &vp_draft_bnode_map, &original_vcs_bnode_map)?;
    println!("extended deanon map:");
    for (f, t) in &extended_deanon_map {
        println!("{}: {}", f.to_string(), t.to_string());
    }
    println!("");

    // reorder the original VC graphs and proof values
    // according to the order of canonicalized graph names of disclosed VCs
    let (original_vc_vec, disclosed_vc_vec, vc_proof_values_vec, is_bound_vec) = reorder_vc_graphs(
        &canonicalized_original_vcs,
        &vc_proof_values.iter().map(|s| s.as_str()).collect(),
        &canonicalized_disclosed_vc_graphs,
        &extended_deanon_map,
        &vc_document_graph_names,
    )?;

    println!("canonicalized original VC (sorted):");
    for VerifiableCredentialTriples { document, proof } in &original_vc_vec {
        println!(
            "document:\n{}",
            document
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
        println!(
            "proof:\n{}",
            proof
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
    }
    println!("canonicalized disclosed VC (sorted):");
    for VerifiableCredentialTriples { document, proof } in &disclosed_vc_vec {
        println!(
            "document:\n{}",
            document
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
        println!(
            "proof:\n{}",
            proof
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
    }

    // generate index map
    let index_map = gen_index_map(&original_vc_vec, &disclosed_vc_vec, &extended_deanon_map)?;
    println!("index_map:\n{:#?}\n", index_map);

    // derive proof value
    let derived_proof_value = derive_proof_value(
        rng,
        secret,
        original_vc_vec,
        is_bound_vec,
        disclosed_vc_vec,
        public_keys,
        vc_proof_values_vec,
        index_map,
        &vp_draft,
        challenge,
        &blind_sign_request,
        &ppid,
        predicate_graphs,
        circuits,
        &extended_deanon_map,
        &verifiable_encryption_for_uid,
    )?;

    // add derived proof value to VP
    let vp_proof_subject = vp_proof_graph
        .subject_for_predicate_object(TYPE, DATA_INTEGRITY_PROOF)
        .ok_or(RDFProofsError::InvalidVP)?;
    let vp_proof_value_quad = QuadRef::new(
        vp_proof_subject,
        PROOF_VALUE,
        LiteralRef::new_typed_literal(&derived_proof_value, MULTIBASE),
        vp_proof_graph_name,
    );
    let mut canonicalized_vp_quads = vp_draft.into_iter().collect::<Vec<_>>();
    canonicalized_vp_quads.push(vp_proof_value_quad);

    Ok(Dataset::from_iter(canonicalized_vp_quads))
}

pub fn derive_proof_string<R: RngCore>(
    rng: &mut R,
    vc_pairs: &Vec<VcPairString>,
    deanon_map: &HashMap<String, String>,
    key_graph: &str,
    challenge: Option<&str>,
    domain: Option<&str>,
    secret: Option<&[u8]>,
    blind_sign_request: Option<BlindSignRequestString>,
    with_ppid: Option<bool>,
    predicates: Option<&Vec<String>>,
    circuits: Option<&HashMap<String, CircuitString>>,
    opener_pub_key: Option<ElGamalPublicKey>,
) -> Result<String, RDFProofsError> {
    // construct inputs for `derive_proof` from string-based inputs
    let vc_pairs = vc_pairs
        .iter()
        .map(|pair| {
            Ok(VcPair::new(
                get_vc_from_ntriples(&pair.original_document, &pair.original_proof)?,
                get_vc_from_ntriples(&pair.disclosed_document, &pair.disclosed_proof)?,
            ))
        })
        .collect::<Result<Vec<_>, RDFProofsError>>()?;
    let deanon_map = get_deanon_map_from_string(deanon_map)?;
    let key_graph = get_graph_from_ntriples(key_graph)?.into();
    let blind_sign_request = if let Some(req) = blind_sign_request {
        Some(BlindSignRequest {
            commitment: multibase_to_ark(&req.commitment)?,
            blinding: multibase_to_ark(&req.blinding)?,
            pok_for_commitment: if let Some(s) = req.pok_for_commitment {
                Some(multibase_to_ark(&s)?)
            } else {
                None
            },
        })
    } else {
        None
    };
    let predicates = match predicates {
        None => vec![],
        Some(predicates) => predicates
            .iter()
            .map(|predicate| Ok(get_graph_from_ntriples(predicate)?))
            .collect::<Result<Vec<_>, RDFProofsError>>()?,
    };

    let circuits = match circuits {
        None => HashMap::new(),
        Some(circuits) => circuits
            .iter()
            .map(|(circuit_id, circuit_str)| {
                let circuit = Circuit::new(
                    &circuit_str.circuit_r1cs,
                    &circuit_str.circuit_wasm,
                    &circuit_str.snark_proving_key,
                )?;
                Ok((NamedNode::new(circuit_id)?, circuit))
            })
            .collect::<Result<HashMap<_, _>, RDFProofsError>>()?,
    };

    let derived_proof = derive_proof(
        rng,
        &vc_pairs,
        &deanon_map,
        &key_graph,
        challenge,
        domain,
        secret,
        blind_sign_request,
        with_ppid,
        predicates,
        circuits,
        opener_pub_key,
    )?;

    Ok(rdf_canon::serialize(&derived_proof))
}

fn get_ppid(
    domain: &Option<&str>,
    secret: &Option<&[u8]>,
    with_nym: Option<bool>,
) -> Result<Option<PPID>, RDFProofsError> {
    let with_nym = match with_nym {
        Some(v) => v,
        None => false,
    };

    if !with_nym {
        return Ok(None);
    }

    if let (Some(domain), Some(secret)) = (domain, secret) {
        Ok(Some(generate_ppid(domain, secret)?))
    } else {
        Err(RDFProofsError::MissingSecretOrDomain)
    }
}

fn get_encrypted_secret_and_pok<R: RngCore>(
    opener_pub_key: &ElGamalPublicKey,
    secret: &[u8],
    rng: &mut R,
) -> Result<ElGamalVerifiableEncryption, RDFProofsError> {
    let params = generate_params(1);
    let hasher = get_hasher();
    let secret = hash_byte_to_field(secret, &hasher).unwrap();
    Ok(elliptic_elgamal_verifiable_encryption_with_bbs_plus(
        &opener_pub_key,
        &params.h[0],
        &secret,
        rng,
    )?)
}

fn get_deanon_map_from_string(
    deanon_map_string: &HashMap<String, String>,
) -> Result<HashMap<NamedOrBlankNode, Term>, RDFProofsError> {
    deanon_map_string
        .iter()
        .map(|(k, v)| {
            let key: NamedOrBlankNode = match get_term_from_string(k)? {
                Term::NamedNode(n) => Ok(n.into()),
                Term::BlankNode(n) => Ok(n.into()),
                Term::Literal(_) => Err(RDFProofsError::InvalidDeanonMapFormat(k.to_string())),
            }?;
            let value = get_term_from_string(v)?;
            Ok((key, value))
        })
        .collect()
}

fn get_public_keys(
    proof_graph: &Graph,
    key_graph: &KeyGraph,
) -> Result<BBSPlusPublicKey, RDFProofsError> {
    let vm_triple = proof_graph
        .triples_for_predicate(VERIFICATION_METHOD)
        .next()
        .ok_or(RDFProofsError::InvalidVerificationMethod)?;
    let vm = match vm_triple.object {
        TermRef::NamedNode(v) => Ok(v),
        _ => Err(RDFProofsError::InvalidVerificationMethodURL),
    }?;
    key_graph.get_public_key(vm)
}

fn deanonymize_subject(
    deanon_map: &HashMap<NamedOrBlankNode, Term>,
    subject: &mut Subject,
) -> Result<(), RDFProofsError> {
    match subject {
        Subject::BlankNode(bnode) => {
            if let Some(v) = deanon_map.get(&NamedOrBlankNode::BlankNode(bnode.clone())) {
                match v {
                    Term::NamedNode(n) => *subject = Subject::NamedNode(n.clone()),
                    Term::BlankNode(n) => *subject = Subject::BlankNode(n.clone()),
                    _ => return Err(RDFProofsError::DeAnonymization),
                }
            }
        }
        Subject::NamedNode(node) => {
            if let Some(v) = deanon_map.get(&NamedOrBlankNode::NamedNode(node.clone())) {
                match v {
                    Term::NamedNode(n) => *subject = Subject::NamedNode(n.clone()),
                    Term::BlankNode(n) => *subject = Subject::BlankNode(n.clone()),
                    _ => return Err(RDFProofsError::DeAnonymization),
                }
            }
        }
        #[cfg(feature = "rdf-star")]
        Subject::Triple(_) => return Err(RDFProofsError::DeAnonymization),
    };
    Ok(())
}

fn deanonymize_named_node(
    deanon_map: &HashMap<NamedOrBlankNode, Term>,
    predicate: &mut NamedNode,
) -> Result<(), RDFProofsError> {
    if let Some(v) = deanon_map.get(&NamedOrBlankNode::NamedNode(predicate.clone())) {
        match v {
            Term::NamedNode(n) => *predicate = n.clone(),
            _ => return Err(RDFProofsError::DeAnonymization),
        }
    }
    Ok(())
}

fn deanonymize_term(
    deanon_map: &HashMap<NamedOrBlankNode, Term>,
    term: &mut Term,
) -> Result<(), RDFProofsError> {
    match term {
        Term::BlankNode(bnode) => {
            if let Some(v) = deanon_map.get(&NamedOrBlankNode::BlankNode(bnode.clone())) {
                *term = v.clone();
            }
        }
        Term::NamedNode(node) => {
            if let Some(v) = deanon_map.get(&NamedOrBlankNode::NamedNode(node.clone())) {
                match v {
                    Term::NamedNode(n) => *term = Term::NamedNode(n.clone()),
                    Term::BlankNode(n) => *term = Term::BlankNode(n.clone()),
                    _ => return Err(RDFProofsError::DeAnonymization),
                }
            }
        }
        Term::Literal(_) => (),
        #[cfg(feature = "rdf-star")]
        Term::Triple(_) => return Err(RDFProofsError::DeAnonymization),
    };
    Ok(())
}

fn canonicalize_vcs(
    vcs: &Vec<VerifiableCredential>,
) -> Result<(Vec<VerifiableCredential>, HashMap<String, String>), RDFProofsError> {
    let mut bnode_map = HashMap::new();
    let canonicalized_vcs = vcs
        .iter()
        .map(|VerifiableCredential { document, proof }| {
            let (canonicalized_document, document_bnode_map) = canonicalize_graph(document)?;
            let (canonicalized_proof, proof_bnode_map) = canonicalize_graph(proof)?;
            for (k, v) in &document_bnode_map {
                if bnode_map.contains_key(k) {
                    return Err(RDFProofsError::BlankNodeCollision);
                } else {
                    bnode_map.insert(k.to_string(), v.to_string());
                }
            }
            for (k, v) in &proof_bnode_map {
                if bnode_map.contains_key(k) {
                    return Err(RDFProofsError::BlankNodeCollision);
                } else {
                    bnode_map.insert(k.to_string(), v.to_string());
                }
            }

            Ok(VerifiableCredential::new(
                canonicalized_document,
                canonicalized_proof,
            ))
        })
        .collect::<Result<Vec<_>, RDFProofsError>>()?;
    Ok((canonicalized_vcs, bnode_map))
}

fn build_vp(
    disclosed_vcs: Vec<VerifiableCredential>,
    challenge: &Option<&str>,
    domain: &Option<&str>,
    blind_sign_request: &Option<BlindSignRequest>,
    ppid: &Option<PPID>,
    encrypted_uid: &Option<ElGamalCiphertext>,
    predicates: Vec<Graph>,
) -> Result<(Dataset, HashMap<String, String>, Vec<BlankNode>), RDFProofsError> {
    let vp_id = BlankNode::default();
    let vp_proof_id = BlankNode::default();
    let vp_proof_graph_id = BlankNode::default();

    let mut vp = Dataset::default();
    vp.insert(QuadRef::new(
        &vp_id,
        TYPE,
        VERIFIABLE_PRESENTATION_TYPE,
        GraphNameRef::DefaultGraph,
    ));
    vp.insert(QuadRef::new(
        &vp_id,
        PROOF,
        &vp_proof_graph_id,
        GraphNameRef::DefaultGraph,
    ));
    vp.insert(QuadRef::new(
        &vp_proof_id,
        TYPE,
        DATA_INTEGRITY_PROOF,
        &vp_proof_graph_id,
    ));
    vp.insert(QuadRef::new(
        &vp_proof_id,
        CRYPTOSUITE,
        LiteralRef::new_simple_literal(CRYPTOSUITE_PROOF),
        &vp_proof_graph_id,
    ));
    vp.insert(QuadRef::new(
        &vp_proof_id,
        PROOF_PURPOSE,
        AUTHENTICATION,
        &vp_proof_graph_id,
    ));
    vp.insert(QuadRef::new(
        &vp_proof_id,
        CREATED,
        LiteralRef::new_typed_literal(&format!("{:?}", Utc::now()), xsd::DATE_TIME),
        &vp_proof_graph_id,
    ));

    // add challenge if exists
    if let Some(challenge) = challenge {
        vp.insert(QuadRef::new(
            &vp_proof_id,
            CHALLENGE,
            LiteralRef::new_simple_literal(*challenge),
            &vp_proof_graph_id,
        ));
    }

    // add domain if exists
    if let Some(domain) = domain {
        vp.insert(QuadRef::new(
            &vp_proof_id,
            DOMAIN,
            LiteralRef::new_simple_literal(*domain),
            &vp_proof_graph_id,
        ));
    }

    // use PPID as holder's ID if it is given, otherwise blank node is used,
    // and add secret commitment if exists
    match (ppid, blind_sign_request) {
        (None, None) => (),
        (None, Some(req)) => {
            let vp_holder_id = BlankNode::default();
            vp.insert(QuadRef::new(
                &vp_id,
                HOLDER,
                &vp_holder_id,
                GraphNameRef::DefaultGraph,
            ));
            vp.insert(QuadRef::new(
                &vp_holder_id,
                SECRET_COMMITMENT,
                LiteralRef::new_typed_literal(&ark_to_base64url(&req.commitment)?, MULTIBASE),
                GraphNameRef::DefaultGraph,
            ));
        }
        (Some(ppid), _) => {
            let nym_multibase = ark_to_base64url(&ppid.ppid)?;
            let vp_holder_id = NamedNode::new(format!("{}{}", PPID_PREFIX, nym_multibase))?;
            vp.insert(QuadRef::new(
                &vp_id,
                HOLDER,
                &vp_holder_id,
                GraphNameRef::DefaultGraph,
            ));
            if let Some(req) = blind_sign_request {
                vp.insert(QuadRef::new(
                    &vp_holder_id,
                    SECRET_COMMITMENT,
                    LiteralRef::new_typed_literal(&ark_to_base64url(&req.commitment)?, MULTIBASE),
                    GraphNameRef::DefaultGraph,
                ));
            }
        }
    }

    // add encrypted uid if exists
    if let Some(encrypted_uid) = encrypted_uid {
        vp.insert(QuadRef::new(
            &vp_proof_id,
            ENCRYPTED_UID,
            LiteralRef::new_simple_literal(&ark_to_base64url(encrypted_uid).unwrap()),
            &vp_proof_graph_id,
        ));
    }

    // add predicates if exist
    for predicate in predicates {
        let predicate_graph_id = BlankNode::default();
        vp.insert(QuadRef::new(
            &vp_id,
            PREDICATE,
            &predicate_graph_id,
            GraphNameRef::DefaultGraph,
        ));
        for triple in predicate.iter() {
            vp.insert(QuadRef::new(
                triple.subject,
                triple.predicate,
                triple.object,
                &predicate_graph_id,
            ));
        }
    }

    // convert disclosed VC graphs (triples) into disclosed VC dataset (quads)
    let mut disclosed_vc_document_graph_names = Vec::with_capacity(disclosed_vcs.len());
    let disclosed_vc_quads = disclosed_vcs
        .iter()
        .map(|disclosed_vc| {
            // generate random blank nodes as graph names
            let disclosed_vc_document_graph_name = BlankNode::default();
            let disclosed_vc_proof_graph_name = BlankNode::default();

            disclosed_vc_document_graph_names.push(disclosed_vc_document_graph_name.clone());

            let disclosed_vc_document_id = disclosed_vc
                .document
                .subject_for_predicate_object(TYPE, VERIFIABLE_CREDENTIAL_TYPE)
                .ok_or(RDFProofsError::VCWithoutVCType)?;

            let mut disclosed_vc_document_quads: Vec<Quad> = disclosed_vc
                .document
                .iter()
                .map(|t| {
                    t.into_owned()
                        .in_graph(disclosed_vc_document_graph_name.clone())
                })
                .collect();

            // add `proof` link from VC document to VC proof graph
            disclosed_vc_document_quads.push(Quad::new(
                disclosed_vc_document_id,
                PROOF,
                disclosed_vc_proof_graph_name.clone(),
                disclosed_vc_document_graph_name.clone(),
            ));

            let mut proof_quads: Vec<Quad> = disclosed_vc
                .get_proof_config()
                .iter()
                .map(|t| {
                    t.into_owned()
                        .in_graph(disclosed_vc_proof_graph_name.clone())
                })
                .collect();
            disclosed_vc_document_quads.append(&mut proof_quads);

            Ok((
                disclosed_vc_document_graph_name,
                disclosed_vc_document_quads,
            ))
        })
        .collect::<Result<Vec<_>, RDFProofsError>>()?;

    // merge VC dataset into VP draft
    for (disclosed_vc_graph_name, disclosed_vc_quad) in disclosed_vc_quads {
        vp.insert(QuadRef::new(
            &vp_id,
            VERIFIABLE_CREDENTIAL,
            &disclosed_vc_graph_name,
            GraphNameRef::DefaultGraph,
        ));
        vp.extend(disclosed_vc_quad);
    }

    println!("vp draft (before canonicalization):\n{}\n", vp.to_string());

    // canonicalize VP draft
    let canonicalized_vp_bnode_map = rdf_canon::issue(&vp)?;
    let canonicalized_vp = rdf_canon::relabel(&vp, &canonicalized_vp_bnode_map)?;
    println!("VP draft bnode map:\n{:#?}\n", canonicalized_vp_bnode_map);
    println!("VP draft:\n{}", rdf_canon::serialize(&canonicalized_vp));

    Ok((
        canonicalized_vp,
        canonicalized_vp_bnode_map,
        disclosed_vc_document_graph_names,
    ))
}

fn extend_deanon_map(
    deanon_map: &HashMap<NamedOrBlankNode, Term>,
    vp_draft_bnode_map: &HashMap<String, String>,
    original_vcs_bnode_map: &HashMap<String, String>,
) -> Result<HashMap<NamedOrBlankNode, Term>, RDFProofsError> {
    // blank node -> original term
    let mut res = vp_draft_bnode_map
        .into_iter()
        .map(|(bnid, cnid)| {
            let mapped_bnid = match original_vcs_bnode_map.get(bnid) {
                Some(v) => v,
                None => bnid,
            };
            let bnode = BlankNode::new(mapped_bnid)?;
            let cnid = NamedOrBlankNode::BlankNode(BlankNode::new(cnid)?);
            if let Some(v) = deanon_map.get(&NamedOrBlankNode::BlankNode(bnode.clone())) {
                Ok((cnid, v.clone()))
            } else {
                Ok((cnid, bnode.into()))
            }
        })
        .collect::<Result<HashMap<_, _>, RDFProofsError>>()?;

    // named node -> original term
    for (k, v) in deanon_map {
        if let NamedOrBlankNode::NamedNode(_) = k {
            res.insert(k.clone(), v.clone());
        }
    }
    Ok(res)
}

fn reorder_vc_graphs(
    canonicalized_original_vcs: &Vec<VerifiableCredential>,
    proof_values: &Vec<&str>,
    canonicalized_disclosed_vc_graphs: &OrderedVerifiableCredentialGraphViews,
    extended_deanon_map: &HashMap<NamedOrBlankNode, Term>,
    vc_document_graph_names: &Vec<BlankNode>,
) -> Result<
    (
        Vec<VerifiableCredentialTriples>,
        Vec<VerifiableCredentialTriples>,
        Vec<String>,
        Vec<bool>,
    ),
    RDFProofsError,
> {
    let mut ordered_original_vcs = BTreeMap::new();
    let mut ordered_proof_values = BTreeMap::new();
    let mut ordered_is_bounds = BTreeMap::new();

    for k in canonicalized_disclosed_vc_graphs.keys() {
        let canonicalized_disclosed_vc_graph_name: &GraphNameRef = k.into();
        let original_vc_graph_name = match canonicalized_disclosed_vc_graph_name {
            GraphNameRef::BlankNode(n) => match extended_deanon_map.get(&(*n).into()) {
                Some(Term::BlankNode(n)) => Ok(n),
                _ => Err(RDFProofsError::Other("invalid VC graph name".to_string())),
            },
            _ => Err(RDFProofsError::Other("invalid VC graph name".to_string())),
        }?;
        let original_index = vc_document_graph_names
            .iter()
            .position(|v| v == original_vc_graph_name)
            .ok_or(RDFProofsError::Other("invalid VC index".to_string()))?;
        let original_vc = canonicalized_original_vcs
            .get(original_index)
            .ok_or(RDFProofsError::Other("invalid VC index".to_string()))?;
        let is_bound = original_vc.is_bound()?;
        let proof_value = proof_values
            .get(original_index)
            .ok_or(RDFProofsError::Other(
                "invalid proof value index".to_string(),
            ))?;
        ordered_original_vcs.insert(k.clone(), original_vc);
        ordered_proof_values.insert(k.clone(), proof_value.to_owned());
        ordered_is_bounds.insert(k.clone(), is_bound);
    }

    // assert the keys of two VC graphs are equivalent
    if !ordered_original_vcs
        .keys()
        .eq(canonicalized_disclosed_vc_graphs.keys())
    {
        return Err(RDFProofsError::Other(
            "the graph names of original and disclosed VC must be equivalent".to_string(),
        ));
    }

    // convert to Vecs
    let original_vc_vec = ordered_original_vcs
        .into_iter()
        .map(|(_, v)| v.into())
        .collect::<Vec<VerifiableCredentialTriples>>();
    let disclosed_vc_vec = canonicalized_disclosed_vc_graphs
        .into_iter()
        .map(|(_, v)| v.into())
        .collect::<Vec<VerifiableCredentialTriples>>();
    let vc_proof_values_vec = ordered_proof_values
        .into_iter()
        .map(|(_, v)| v.into())
        .collect::<Vec<_>>();
    let is_bound_vec = ordered_is_bounds
        .into_iter()
        .map(|(_, v)| v)
        .collect::<Vec<_>>();

    Ok((
        original_vc_vec,
        disclosed_vc_vec,
        vc_proof_values_vec,
        is_bound_vec,
    ))
}

fn gen_index_map(
    original_vc_vec: &Vec<VerifiableCredentialTriples>,
    disclosed_vc_vec: &Vec<VerifiableCredentialTriples>,
    extended_deanon_map: &HashMap<NamedOrBlankNode, Term>,
) -> Result<Vec<StatementIndexMap>, RDFProofsError> {
    let mut disclosed_vc_triples_cloned = (*disclosed_vc_vec).clone();

    // deanonymize each disclosed VC triples, keeping their orders
    for VerifiableCredentialTriples { document, proof } in &mut disclosed_vc_triples_cloned {
        for triple in document.into_iter() {
            deanonymize_subject(extended_deanon_map, &mut triple.subject)?;
            deanonymize_named_node(extended_deanon_map, &mut triple.predicate)?;
            deanonymize_term(extended_deanon_map, &mut triple.object)?;
        }
        for triple in proof.into_iter() {
            deanonymize_subject(extended_deanon_map, &mut triple.subject)?;
            deanonymize_named_node(extended_deanon_map, &mut triple.predicate)?;
            deanonymize_term(extended_deanon_map, &mut triple.object)?;
        }
    }
    println!("deanonymized canonicalized disclosed VC graphs:");
    for VerifiableCredentialTriples { document, proof } in &disclosed_vc_triples_cloned {
        println!(
            "document:\n{}",
            document
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
        println!(
            "proof:\n{}",
            proof
                .iter()
                .map(|t| format!("{} .\n", t.to_string()))
                .reduce(|l, r| format!("{}{}", l, r))
                .unwrap()
        );
    }

    // calculate index mapping
    let index_map = disclosed_vc_triples_cloned
        .iter()
        .zip(original_vc_vec)
        .map(
            |(
                VerifiableCredentialTriples {
                    document: disclosed_document,
                    proof: disclosed_proof,
                },
                VerifiableCredentialTriples {
                    document: original_document,
                    proof: original_proof,
                },
            )| {
                let document_map = disclosed_document
                    .iter()
                    .map(|disclosed_triple| {
                        original_document
                            .iter()
                            .position(|original_triple| *disclosed_triple == *original_triple)
                            .ok_or(RDFProofsError::DisclosedVCIsNotSubsetOfOriginalVC)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let proof_map = disclosed_proof
                    .iter()
                    .map(|disclosed_triple| {
                        original_proof
                            .iter()
                            .position(|original_triple| *disclosed_triple == *original_triple)
                            .ok_or(RDFProofsError::DisclosedVCIsNotSubsetOfOriginalVC)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let document_len = original_document.len();
                let proof_len = original_proof.len();
                Ok(StatementIndexMap::new(
                    document_map,
                    document_len,
                    proof_map,
                    proof_len,
                ))
            },
        )
        .collect::<Result<Vec<_>, RDFProofsError>>()?;

    Ok(index_map)
}

fn derive_proof_value<R: RngCore>(
    rng: &mut R,
    secret: Option<&[u8]>,
    original_vc_triples: Vec<VerifiableCredentialTriples>,
    is_bounds: Vec<bool>,
    disclosed_vc_triples: Vec<VerifiableCredentialTriples>,
    public_keys: Vec<BBSPlusPublicKey>,
    proof_values: Vec<String>,
    index_map: Vec<StatementIndexMap>,
    canonicalized_vp: &Dataset,
    challenge: Option<&str>,
    blind_sign_request: &Option<BlindSignRequest>,
    ppid: &Option<PPID>,
    predicate_graphs: OrderedGraphViews,
    circuits: HashMap<NamedNode, Circuit>,
    extended_deanon_map: &HashMap<NamedOrBlankNode, Term>,
    verifiable_encryption_for_uid: &Option<ElGamalVerifiableEncryption>,
) -> Result<String, RDFProofsError> {
    let hasher = get_hasher();

    // reorder disclosed VC triples according to index map
    let reordered_disclosed_vc_triples = reorder_vc_triples(&disclosed_vc_triples, &index_map)?;
    println!(
        "reordered_disclosed_vc_triples:\n{:#?}\n",
        reordered_disclosed_vc_triples
    );

    // identify disclosed and undisclosed terms
    let disclosed_and_undisclosed_terms = reordered_disclosed_vc_triples
        .iter()
        .zip(original_vc_triples)
        .zip(&is_bounds)
        .enumerate()
        .map(
            |(i, ((disclosed_vc_triples, original_vc_triples), is_bound))| {
                let s = match (is_bound, secret) {
                    (true, Some(s)) => Ok(Some(s)),
                    (true, None) => Err(RDFProofsError::MissingSecret),
                    (false, _) => Ok(None),
                }?;
                get_disclosed_and_undisclosed_terms(
                    disclosed_vc_triples,
                    &original_vc_triples,
                    i,
                    s,
                    &hasher,
                )
            },
        )
        .collect::<Result<Vec<_>, RDFProofsError>>()?;
    println!(
        "disclosed_and_undisclosed:\n{:#?}\n",
        disclosed_and_undisclosed_terms
    );
    println!("proof values: {:?}", proof_values);

    let term_counts = disclosed_and_undisclosed_terms
        .iter()
        .map(|t| {
            t.term_count
                .try_into()
                .map_err(|_| RDFProofsError::MessageSizeOverflow)
        })
        .collect::<Result<Vec<u32>, _>>()?;
    let params_for_commitment = generate_params(1);
    let params_and_pks = term_counts
        .iter()
        .zip(public_keys)
        .map(|(t, pk)| (generate_params(*t), pk));

    // merge each partial equivs
    let mut equivs: BTreeMap<OrderedNamedOrBlankNode, Vec<(usize, usize)>> = BTreeMap::new();
    for DisclosedAndUndisclosedTerms {
        equivs: partial_equivs,
        ..
    } in &disclosed_and_undisclosed_terms
    {
        for (k, v) in partial_equivs {
            equivs
                .entry(k.clone().into())
                .or_default()
                .extend(v.clone());
        }
    }

    // build statements
    let mut statements = Statements::new();
    // statements for BBS+ signatures
    for (DisclosedAndUndisclosedTerms { disclosed, .. }, (params, public_key)) in
        disclosed_and_undisclosed_terms.iter().zip(params_and_pks)
    {
        statements.add(PoKBBSPlusStmt::new_statement_from_params(
            params.clone(),
            public_key,
            disclosed.clone(),
        ));
    }
    // statement for PPID
    let mut ppid_index = None;
    if let Some(ppid) = ppid {
        statements.add(PedersenCommitmentStmt::new_statement_from_params(
            vec![ppid.base],
            ppid.ppid,
        ));
        ppid_index = Some(statements.len() - 1);
    }
    // statements for verifiable encryption of uid
    if let Some(verifiable_encryption_for_uid) = verifiable_encryption_for_uid {
        for statement in verifiable_encryption_for_uid.statements.0.iter() {
            statements.add(statement.clone());
        }
    }
    // statement for secret commitment
    let mut secret_commitment_index = None;
    if let Some(req) = blind_sign_request {
        statements.add(PedersenCommitmentStmt::new_statement_from_params(
            vec![params_for_commitment.h_0, params_for_commitment.h[0]],
            req.commitment,
        ));
        secret_commitment_index = Some(statements.len() - 1);
    }
    // statements for predicates
    let mut predicate_indexes = vec![];
    let mut predicate_privates = vec![];
    let mut predicate_publics = vec![];
    for (_, predicate_graph) in predicate_graphs {
        let predicate_subject = predicate_graph
            .subject_for_predicate_object(TYPE, PREDICATE_TYPE)
            .ok_or(RDFProofsError::InvalidPredicate)?;
        let TermRef::NamedNode(predicate_circuit) = predicate_graph
            .object_for_subject_predicate(predicate_subject, CIRCUIT)
            .ok_or(RDFProofsError::InvalidPredicate)?
        else {
            return Err(RDFProofsError::InvalidPredicate);
        };
        let circuit = circuits
            .get(&predicate_circuit.into_owned())
            .ok_or(RDFProofsError::MissingPredicateCircuit)?;
        statements.add(R1CSCircomProver::new_statement_from_params(
            circuit.get_r1cs(),
            circuit.get_wasm(),
            circuit.get_proving_key(),
        )?);
        predicate_indexes.push(statements.len() - 1);

        let mut privates = vec![];
        let TermRef::BlankNode(predicate_private) = predicate_graph
            .object_for_subject_predicate(predicate_subject, PRIVATE)
            .ok_or(RDFProofsError::InvalidPredicate)?
        else {
            return Err(RDFProofsError::InvalidPredicate);
        };
        read_private_var_list(predicate_private, &mut privates, &predicate_graph)?;
        predicate_privates.push(privates);

        let mut publics = vec![];
        let TermRef::BlankNode(predicate_public) = predicate_graph
            .object_for_subject_predicate(predicate_subject, PUBLIC)
            .ok_or(RDFProofsError::InvalidPredicate)?
        else {
            return Err(RDFProofsError::InvalidPredicate);
        };
        read_public_var_list(predicate_public, &mut publics, &predicate_graph)?;
        predicate_publics.push(publics);
    }

    // build meta statements
    let mut meta_statements = MetaStatements::new();

    // proof of equality for embedded secrets
    let mut secret_equiv_set: BTreeSet<(usize, usize)> = is_bounds
        .iter()
        .enumerate()
        .filter(|(_, &is_bound)| is_bound)
        .map(|(i, _)| (i, 0)) // `0` is the index for embedded secret in VC
        .collect();
    // add PPID to the proof of equalities if exists
    if let Some(idx) = ppid_index {
        // `0` corresponds to the committed secret in PPID
        secret_equiv_set.insert((idx, 0));
    }
    // add secret commitment to the proof of equalities if exists
    if let Some(idx) = secret_commitment_index {
        // `1` corresponds to the committed secret in Pedersen Commitment (`0` corresponds to the blinding)
        secret_equiv_set.insert((idx, 1));
    }
    if secret_equiv_set.len() > 1 {
        meta_statements.add_witness_equality(EqualWitnesses(secret_equiv_set));
    }

    // proof of equality
    for (equiv_c14n_id, equiv_vec) in equivs {
        // add equality for attributes in credentials
        let mut equiv_set: BTreeSet<(usize, usize)> = equiv_vec.into_iter().collect();

        // add equality for predicate private variables
        for (predicate_private, predicate_index) in
            predicate_privates.iter().zip(&predicate_indexes)
        {
            if let Some(idx_in_predicate) = predicate_private
                .iter()
                .position(|(_, bnode_in_private)| *bnode_in_private == equiv_c14n_id.0)
            {
                equiv_set.insert((*predicate_index, idx_in_predicate));
            }
        }
        println!("equiv_set: {:?}", equiv_set);
        if equiv_set.len() > 1 {
            meta_statements.add_witness_equality(EqualWitnesses(equiv_set));
        }
    }
    println!("meta_statements: {:?}", meta_statements);

    // build proof spec
    let context = generate_proof_spec_context(&canonicalized_vp, &index_map)?;
    let proof_spec = ProofSpec::new(statements, meta_statements, vec![], Some(context));
    proof_spec.validate()?;

    // build witnesses
    let mut witnesses = Witnesses::new();
    // witnesses for BBS+ signatures
    for (DisclosedAndUndisclosedTerms { undisclosed, .. }, proof_value) in
        disclosed_and_undisclosed_terms.iter().zip(proof_values)
    {
        let signature: BBSPlusSignature = multibase_to_ark(&proof_value)?;
        witnesses.add(PoKBBSPlusWit::new_as_witness(
            signature,
            undisclosed.clone(),
        ));
    }
    // witness for PPID
    if ppid.is_some() {
        if let Some(s) = secret {
            witnesses.add(Witness::PedersenCommitment(vec![hash_byte_to_field(
                s, &hasher,
            )?]));
        } else {
            return Err(RDFProofsError::MissingSecret);
        }
    }
    // witness for verifiable encryption of uid
    if let Some(verifiable_encryption_for_uid) = verifiable_encryption_for_uid {
        for witness in verifiable_encryption_for_uid.witnesses.0.iter() {
            witnesses.add(witness.clone());
        }
    }
    // witness for secret commitment
    if let Some(req) = blind_sign_request {
        if let Some(s) = secret {
            witnesses.add(Witness::PedersenCommitment(vec![
                req.blinding,
                hash_byte_to_field(s, &hasher)?,
            ]));
        } else {
            return Err(RDFProofsError::MissingSecret);
        }
    }
    // witness for predicates
    for (private, public) in predicate_privates.iter().zip(&predicate_publics) {
        let mut r1cs_wit = R1CSCircomWitness::new();
        // private
        for (var, val) in private {
            println!("{}", val);
            let val = extended_deanon_map
                .get(val)
                .ok_or(RDFProofsError::InvalidPredicate)?;
            r1cs_wit.set_private(
                var.to_string(),
                vec![hash_term_to_field(val.into(), &hasher)?],
            )
        }
        // public
        for (var, val) in public {
            println!("{}", val);
            r1cs_wit.set_public(
                var.to_string(),
                vec![hash_term_to_field(val.into(), &hasher)?],
            )
        }
        witnesses.add(Witness::R1CSLegoGroth16(r1cs_wit));
    }
    println!("witnesses:\n{:#?}\n", witnesses);

    // build proof
    let proof = Proof::new::<R, BBSPlusHash>(
        rng,
        proof_spec,
        witnesses,
        challenge.map(|v| v.as_bytes().to_vec()), // TODO: consider if it is required as it's already included in `proof_spec.context`
        Default::default(),
    )?
    .0;
    println!("proof:\n{:#?}\n", proof);

    // serialize proof and index_map
    serialize_proof_with_index_map(proof, &index_map)
}

fn serialize_proof_with_index_map(
    proof: Proof,
    index_map: &Vec<StatementIndexMap>,
) -> Result<String, RDFProofsError> {
    // TODO: optimize
    // TODO: use multicodec
    let proof_with_index_map = ProofWithIndexMap {
        proof,
        index_map: index_map.clone(),
    };
    let proof_with_index_map_cbor = serde_cbor::to_vec(&proof_with_index_map)?;
    let proof_with_index_map_multibase =
        multibase::encode(Base::Base64Url, proof_with_index_map_cbor);
    Ok(proof_with_index_map_multibase)
}

#[derive(Debug)]
struct DisclosedAndUndisclosedTerms {
    disclosed: BTreeMap<usize, Fr>,
    undisclosed: BTreeMap<usize, Fr>,
    equivs: HashMap<NamedOrBlankNode, Vec<(usize, usize)>>,
    term_count: usize,
}

fn get_disclosed_and_undisclosed_terms(
    disclosed_vc_triples: &DisclosedVerifiableCredential,
    original_vc_triples: &VerifiableCredentialTriples,
    vc_index: usize,
    secret: Option<&[u8]>,
    hasher: &BBSPlusDefaultFieldHasher,
) -> Result<DisclosedAndUndisclosedTerms, RDFProofsError> {
    let mut disclosed_terms = BTreeMap::<usize, Fr>::new();
    let mut undisclosed_terms = BTreeMap::<usize, Fr>::new();
    let mut equivs = HashMap::<NamedOrBlankNode, Vec<(usize, usize)>>::new();

    let DisclosedVerifiableCredential {
        document: disclosed_document,
        proof: disclosed_proof,
    } = disclosed_vc_triples;
    let VerifiableCredentialTriples {
        document: original_document,
        proof: original_proof,
    } = original_vc_triples;

    let mut current_term_index = 0;

    match secret {
        Some(s) => undisclosed_terms.insert(current_term_index, hash_byte_to_field(s, hasher)?),
        None => disclosed_terms.insert(current_term_index, Fr::from(1)),
    };
    current_term_index += 1;

    for (j, disclosed_triple) in disclosed_document {
        let original = original_document
            .get(*j)
            .ok_or(RDFProofsError::DeriveProofValue)?
            .clone();
        build_disclosed_and_undisclosed_terms(
            disclosed_triple,
            current_term_index,
            vc_index,
            &original,
            &mut disclosed_terms,
            &mut undisclosed_terms,
            &mut equivs,
            hasher,
        )?;
        current_term_index += 3;
    }

    let delimiter = get_delimiter()?;
    disclosed_terms.insert(current_term_index, delimiter);
    current_term_index += 1;

    for (j, disclosed_triple) in disclosed_proof {
        let original = original_proof
            .get(*j)
            .ok_or(RDFProofsError::DeriveProofValue)?
            .clone();
        build_disclosed_and_undisclosed_terms(
            disclosed_triple,
            current_term_index,
            vc_index,
            &original,
            &mut disclosed_terms,
            &mut undisclosed_terms,
            &mut equivs,
            hasher,
        )?;
        current_term_index += 3;
    }
    Ok(DisclosedAndUndisclosedTerms {
        disclosed: disclosed_terms,
        undisclosed: undisclosed_terms,
        equivs,
        term_count: current_term_index,
    })
}

fn build_disclosed_and_undisclosed_terms(
    disclosed_triple: &Option<Triple>,
    subject_index: usize,
    vc_index: usize,
    original: &Triple,
    disclosed_terms: &mut BTreeMap<usize, Fr>,
    undisclosed_terms: &mut BTreeMap<usize, Fr>,
    equivs: &mut HashMap<NamedOrBlankNode, Vec<(usize, usize)>>,
    hasher: &BBSPlusDefaultFieldHasher,
) -> Result<(), RDFProofsError> {
    let predicate_index = subject_index + 1;
    let object_index = subject_index + 2;

    let subject_fr = hash_term_to_field((&original.subject).into(), hasher)?;
    let predicate_fr = hash_term_to_field((&original.predicate).into(), hasher)?;
    let object_fr = hash_term_to_field((&original.object).into(), hasher)?;

    match disclosed_triple {
        Some(triple) => {
            match &triple.subject {
                Subject::BlankNode(b) => {
                    undisclosed_terms.insert(subject_index, subject_fr);
                    equivs
                        .entry(NamedOrBlankNode::BlankNode(b.clone().into()))
                        .or_default()
                        .push((vc_index, subject_index));
                }
                Subject::NamedNode(n) if is_nym(n) => {
                    undisclosed_terms.insert(subject_index, subject_fr);
                    equivs
                        .entry(NamedOrBlankNode::NamedNode(n.clone().into()))
                        .or_default()
                        .push((vc_index, subject_index));
                }
                Subject::NamedNode(_) => {
                    disclosed_terms.insert(subject_index, subject_fr);
                }
                #[cfg(feature = "rdf-star")]
                Subject::Triple(_) => return Err(RDFProofsError::RDFStarUnsupported),
            };

            if is_nym(&triple.predicate) {
                undisclosed_terms.insert(predicate_index, predicate_fr);
                equivs
                    .entry(NamedOrBlankNode::NamedNode(triple.predicate.clone().into()))
                    .or_default()
                    .push((vc_index, predicate_index));
            } else {
                disclosed_terms.insert(predicate_index, predicate_fr);
            };

            match &triple.object {
                Term::BlankNode(b) => {
                    undisclosed_terms.insert(object_index, object_fr);
                    equivs
                        .entry(NamedOrBlankNode::BlankNode(b.clone().into()))
                        .or_default()
                        .push((vc_index, object_index));
                }
                Term::NamedNode(n) if is_nym(n) => {
                    undisclosed_terms.insert(object_index, object_fr);
                    equivs
                        .entry(NamedOrBlankNode::NamedNode(n.clone().into()))
                        .or_default()
                        .push((vc_index, object_index));
                }
                Term::NamedNode(_) | Term::Literal(_) => {
                    disclosed_terms.insert(object_index, object_fr);
                }
                #[cfg(feature = "rdf-star")]
                Term::Triple(_) => return Err(RDFProofsError::RDFStarUnsupported),
            };
        }

        None => {
            undisclosed_terms.insert(subject_index, subject_fr);
            undisclosed_terms.insert(predicate_index, predicate_fr);
            undisclosed_terms.insert(object_index, object_fr);
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CircuitString;
    use crate::{
        ark_to_base64url, blind_sign_string, blind_verify_string,
        common::{get_dataset_from_nquads, get_graph_from_ntriples, R1CS},
        derive_proof,
        derive_proof::get_deanon_map_from_string,
        derive_proof_string, elliptic_elgamal_keygen,
        error::RDFProofsError,
        request_blind_sign_string, unblind_string, verify_blind_sign_request_string, verify_proof,
        verify_proof_string, KeyGraph, VcPair, VcPairString, VerifiableCredential,
    };
    use ark_std::rand::{rngs::StdRng, SeedableRng};
    use legogroth16::circom::CircomCircuit;
    use multibase::Base;
    use oxrdf::{NamedOrBlankNode, Term};
    use std::collections::HashMap;

    const KEY_GRAPH: &str = r#"
        # issuer0
        <did:example:issuer0> <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        <did:example:issuer0#bls12_381-g2-pub001> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#Multikey> .
        <did:example:issuer0#bls12_381-g2-pub001> <https://w3id.org/security#controller> <did:example:issuer0> .
        <did:example:issuer0#bls12_381-g2-pub001> <https://w3id.org/security#secretKeyMultibase> "uekl-7abY7R84yTJEJ6JRqYohXxPZPDoTinJ7XCcBkmk" .
        <did:example:issuer0#bls12_381-g2-pub001> <https://w3id.org/security#publicKeyMultibase> "ukiiQxfsSfV0E2QyBlnHTK2MThnd7_-Fyf6u76BUd24uxoDF4UjnXtxUo8b82iuPZBOa8BXd1NpE20x3Rfde9udcd8P8nPVLr80Xh6WLgI9SYR6piNzbHhEVIfgd_Vo9P" .
        # issuer1
        <did:example:issuer1> <https://w3id.org/security#verificationMethod> <did:example:issuer1#bls12_381-g2-pub001> .
        <did:example:issuer1#bls12_381-g2-pub001> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#Multikey> .
        <did:example:issuer1#bls12_381-g2-pub001> <https://w3id.org/security#controller> <did:example:issuer1> .
        <did:example:issuer1#bls12_381-g2-pub001> <https://w3id.org/security#secretKeyMultibase> "uQkpZn0SW42c2tlYa0IIFXyabAYHbwc0z3l_GvXQbWSg" .
        <did:example:issuer1#bls12_381-g2-pub001> <https://w3id.org/security#publicKeyMultibase> "usFM3CcvBMl_Dg5ixhQkHKGdqzY3GU9Uck6lj2i8vpbzLFOiZnjDNOpsItrkbNf2iCku-SZu5kO3nbLis-fuRhz_QwFcKw9IBpbPRPwXNQTX3zzcFsoNzs_wo8tkLQlcS" .
        # issuer2
        <did:example:issuer2> <https://w3id.org/security#verificationMethod> <did:example:issuer2#bls12_381-g2-pub001> .
        <did:example:issuer2#bls12_381-g2-pub001> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#Multikey> .
        <did:example:issuer2#bls12_381-g2-pub001> <https://w3id.org/security#controller> <did:example:issuer2> .
        <did:example:issuer2#bls12_381-g2-pub001> <https://w3id.org/security#secretKeyMultibase> "u4nmBsiSwvHj7i_gBu1L6Cug0OXXhVPF6NWLfkQbCZiU" .
        <did:example:issuer2#bls12_381-g2-pub001> <https://w3id.org/security#publicKeyMultibase> "uo_yMZWlZwQzLqEe6hEsORbsV5cSHQEQHNI0EOe_eUJdHsgCRxtpWMcxxcdshH5pAAUxt_ni6_cQCud3CdMcjAUN8yOvzhuzeIW_H-Dyncdrc3w0f2WxdH3oRcnvPTwrb" .
        # issuer3
        <did:example:issuer3> <https://w3id.org/security#verificationMethod> <did:example:issuer3#bls12_381-g2-pub001> .
        <did:example:issuer3#bls12_381-g2-pub001> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#Multikey> .
        <did:example:issuer3#bls12_381-g2-pub001> <https://w3id.org/security#controller> <did:example:issuer3> .
        <did:example:issuer3#bls12_381-g2-pub001> <https://w3id.org/security#secretKeyMultibase> "uH1yGFG6C1pJd_N45wkOPrSNdvILdLm0c_0AXXRDGZy8" .
        <did:example:issuer3#bls12_381-g2-pub001> <https://w3id.org/security#publicKeyMultibase> "uidSE_Urr5MFE4SoqV3TZTBHPHM-tkpdRhBPrYeIbsudglVV_cddyEstHJOmSkfPOFsvEuA9qtWjFNpBebVSS4DPxBfNNWESSCz_vrnH62hbfpWdJSFR8YbqjborvpgM6" .
        "#;

    const VC_1: &str = r#"
        <did:example:john> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        <did:example:john> <http://schema.org/name> "John Smith" .
        <did:example:john> <http://example.org/vocab/isPatientOf> _:b0 .
        <did:example:john> <http://schema.org/worksFor> _:b1 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/lotNumber> "0000001" .
        _:b0 <http://example.org/vocab/vaccinationDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/a> .
        _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/b> .
        _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> .
        _:b1 <http://schema.org/name> "ABC inc." .
        <http://example.org/vcred/00> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#credentialSubject> <did:example:john> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const VC_PROOF_1: &str = r#"
        _:b0 <https://w3id.org/security#proofValue> "ui_TYLyZXnF1LRhdzEDrKiAWA0Tbrm1GmCHXBVnX39BTBnIbdFLc9p2jRAw0H4jzznHL4DdyqBDvkUBbr0eTTUk3vNVI1LRxSfXRqqLng4Qx6SX7tptjtHzjJMkQnolGpiiFfE9k8OhOKcntcJwGSaQ"^^<https://w3id.org/security#multibase> .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const VC_1_MODIFIED: &str = r#"
        <did:example:john> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        <did:example:john> <http://schema.org/name> "**********************************" .  # modified
        <did:example:john> <http://example.org/vocab/isPatientOf> _:b0 .
        <did:example:john> <http://schema.org/worksFor> _:b1 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/lotNumber> "0000001" .
        _:b0 <http://example.org/vocab/vaccinationDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/a> .
        _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/b> .
        _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> .
        _:b1 <http://schema.org/name> "ABC inc." .
        <http://example.org/vcred/00> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#credentialSubject> <did:example:john> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const VC_2: &str = r#"
        <http://example.org/vaccine/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccine> .
        <http://example.org/vaccine/a> <http://schema.org/name> "AwesomeVaccine" .
        <http://example.org/vaccine/a> <http://schema.org/manufacturer> <http://example.org/awesomeCompany> .
        <http://example.org/vaccine/a> <http://schema.org/status> "active" .
        <http://example.org/vicred/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        <http://example.org/vicred/a> <https://www.w3.org/2018/credentials#credentialSubject> <http://example.org/vaccine/a> .
        <http://example.org/vicred/a> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer3> .
        <http://example.org/vicred/a> <https://www.w3.org/2018/credentials#issuanceDate> "2020-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        <http://example.org/vicred/a> <https://www.w3.org/2018/credentials#expirationDate> "2023-12-31T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const _VC_PROOF_WITHOUT_PROOFVALUE_2: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-03T09:49:25Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer3#bls12_381-g2-pub001> .
        "#;
    const VC_PROOF_2: &str = r#"
        _:b0 <https://w3id.org/security#proofValue> "uoB9zdaILAqel15HTh6MtIkDZjoeQn2g-fqACEgZvKNMRbgGqTOmNDclM2Pv-WF7BnHL4DdyqBDvkUBbr0eTTUk3vNVI1LRxSfXRqqLng4Qx6SX7tptjtHzjJMkQnolGpiiFfE9k8OhOKcntcJwGSaQ"^^<https://w3id.org/security#multibase> .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-03T09:49:25Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer3#bls12_381-g2-pub001> .
        "#;
    const DISCLOSED_VC_1: &str = r#"
        _:e0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        _:e0 <http://example.org/vocab/isPatientOf> _:b0 .
        _:e0 <http://schema.org/worksFor> _:b1 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/vaccine> _:e1 .
        _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> .
        _:e2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        _:e2 <https://www.w3.org/2018/credentials#credentialSubject> _:e0 .
        _:e2 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        _:e2 <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:e2 <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const DISCLOSED_VC_PROOF_1: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const DISCLOSED_VC_2: &str = r#"
        _:e1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccine> .
        _:e1 <http://schema.org/status> "active" .
        _:e3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        _:e3 <https://www.w3.org/2018/credentials#credentialSubject> _:e1 .
        _:e3 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer3> .
        _:e3 <https://www.w3.org/2018/credentials#issuanceDate> "2020-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:e3 <https://www.w3.org/2018/credentials#expirationDate> "2023-12-31T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const DISCLOSED_VC_PROOF_2: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-03T09:49:25Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer3#bls12_381-g2-pub001> .
        "#;
    const DEANON_MAP: [(&str, &str); 4] = [
        ("_:e0", "<did:example:john>"),
        ("_:e1", "<http://example.org/vaccine/a>"),
        ("_:e2", "<http://example.org/vcred/00>"),
        ("_:e3", "<http://example.org/vicred/a>"),
    ];
    fn get_example_deanon_map_string() -> HashMap<String, String> {
        DEANON_MAP
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn get_example_deanon_map() -> HashMap<NamedOrBlankNode, Term> {
        get_deanon_map_from_string(&get_example_deanon_map_string()).unwrap()
    }
    const VP: &str = r#"
        _:c14n1 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n10 .
        _:c14n1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> _:c14n10 .
        _:c14n1 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" _:c14n10 .
        _:c14n1 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> _:c14n10 .
        _:c14n1 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> _:c14n10 .
        _:c14n11 <http://purl.org/dc/terms/created> "2023-10-06T05:40:05.941640167Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n13 .
        _:c14n11 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> _:c14n13 .
        _:c14n11 <https://w3id.org/security#challenge> "abcde" _:c14n13 .
        _:c14n11 <https://w3id.org/security#cryptosuite> "bbs-termwise-proof-2023" _:c14n13 .
        _:c14n11 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#authenticationMethod> _:c14n13 .
        _:c14n11 <https://w3id.org/security#proofValue> "uomFhWQnaAgAAAAAAAAAAlWj7ySOnU5mw_rG271EltSGHfh0Sabj6_FBXzZ7GBT7TTXM-OJk2y-UjU8hAYyS9p5SkDSPYysuFjUy9RUNu4ypsNdZrRJX2E3TmTg-186q3jvPKExzchRPg7LrE6pWeqzMXck8jrsaUR1_zIsH6KKuTO5gnDEOV4Ufg8zCm7nDLetSoflGoX0d-AUrr9-C_j1WOnJ0-lOnlLr3Mg5hOjxDktjaDvUdwDJ36rFx7agxBvtUKa7deW1Ta-Vq1vFiiAgAAAAAAAACTBwTiEdk00-BvIIfqz8oDk2xDYnTLtUDSu6P0xdL0RjQuKBHV6xaVhb5qoBYP-IdGKx2-2Sk21iHIjrVlQFBup830cauPb0nhTDonFpZRwL7ncq9hXu62basWvqFM3ThJWXiTXss4OpuJW5D0E5h6JQAAAAAAAAB-w7b_iVOFQAQGHgFpp06QV6gwCTxWfLJyxKxKSK-kHpm0wTrncrlMfDmqvHqaHWlcXvv6p7dTHnX0tJLWYC9XUt2I6oyQBKKHQlBI7ZDXkk0Xq9P7QGXrH48bcAW7dTbHdgGgIIBxJ3n8mg1zvsidRWNMNUJx7nHIWxKWIRe5BGqk0y5xBZvij7h_8R64Vw4RfbjtajslQPUN0fiu3s0qP0Iq0VyjWycJ6uAn8i4kZWNWbafJLH8ki8rz_H_o3zC0n0S_-meUXC7Ok2cVmKmcZrZuC-sjgMLdJbVjhqWUD1LdiOqMkASih0JQSO2Q15JNF6vT-0Bl6x-PG3AFu3U2R3eQBscDGEj2SejHCKIW5AQ3vI2u81WOkaojzcsfMR5S3YjqjJAEoodCUEjtkNeSTRer0_tAZesfjxtwBbt1NlltLFFHMcT568va2232ixBc7DF1IviaZJdNvtrU1nxqWW0sUUcxxPnry9rbbfaLEFzsMXUi-Jpkl02-2tTWfGpS3YjqjJAEoodCUEjtkNeSTRer0_tAZesfjxtwBbt1NlltLFFHMcT568va2232ixBc7DF1IviaZJdNvtrU1nxqWW0sUUcxxPnry9rbbfaLEFzsMXUi-Jpkl02-2tTWfGpZbSxRRzHE-evL2ttt9osQXOwxdSL4mmSXTb7a1NZ8anaD_nngpqe5xaQdji-u3evtHCq1bi56QEEZ6eyhzJhQhV79oieifauuxhKLxl3re4PJGFwyCXpXH6Bgi7eedVhKPDD2T3YTIj22AH4inhAyV368j4d3voLJAa4inSwSXUd3kAbHAxhI9knoxwiiFuQEN7yNrvNVjpGqI83LHzEe17WRQdZOAXg-5RYkYZm1ubixq0p0HoNmrqa-L4hCSjEg-SkiGOJYUfBxFAgkYTauH2E2FLz5_NHR4g0mxD2_NcVmZ1okq1eLeIdzUV9RPd4cUbr5uiGObyTu2miM3SsUboOemDnsRSTr7yilMe1x8WK00oIqAJ3tLt71sr2fjSjdAXrIJwtaz3pK0--RrrgOzC2OWjzZWOna0i5WVnhcZZ_SUYwus1fQv3H5VCsXgIA0Sztj5kM0msSkvOQ9sc4bx3YBoCCAcSd5_JoNc77InUVjTDVCce5xyFsSliEXuQRUD-bXOni6wTd1ZJKGXVsu3cDKGrwRcMLtUcLCmo-bCLW73RuCJOGzemtQ-WQtWEtPjfuwiuVWJkV5o152B1BpBgNslfYVji5qEQYcG9iz0suNjVlTC8mcKN4vDspNBW71xFFH-CBvYGoSHgKZ3l0tIJylcX5FMV_FjZO03bLqO8d2AaAggHEnefyaDXO-yJ1FY0w1QnHucchbEpYhF7kELS0TeuolCiEnceAsJagqrgL7AE3FaIpDybD_b6xVaFstLRN66iUKISdx4CwlqCquAvsATcVoikPJsP9vrFVoWy0tE3rqJQohJ3HgLCWoKq4C-wBNxWiKQ8mw_2-sVWhbLS0TeuolCiEnceAsJagqrgL7AE3FaIpDybD_b6xVaFstLRN66iUKISdx4CwlqCquAvsATcVoikPJsP9vrFVoWwCMUROvw7h9P0TgM_oleY-MmLKq9MTn6Y29NKPvmCbprfIx7f4jMu8SRN84CZ0fIv6vvzRo0BC2b_XFb218o1weMHOr8tTs_YfpnJa2qoB0WmgC5Z86ol9FQqGUYi8WVCixy-FMlRZ6nwjJm_4gCSPEKeFVbmUwkqR79KdKDvuMkmrzSX20mYUp1fD6TgghZk2PRSnarn64FqMNRlkwc2K0P30L2iySXPeJEqAUf8TUekHwtVLTyDz_a9Obmz9lYuwCAAAAAAAAAOlK1niDJgDT9pxIwsMeCg3yNSt7nzQ_Ja30N2HYcts0K3EQ_3v04hCcZNINbmYwTZ98XSwMgURCHZaIV-7RvQeiuAl3770_97VvG7-bLPJj35GhJmUwK8Tbw_xBFoMz9yWTIew6ivMP3F-9W2hCyzQVAAAAAAAAAGNy0ZdMnEafcDVeEYYXTavNQsN_5yO8taRWovPJyGUU0quwoJjxTyu_wstf4jKhU32wUodzmFoPOL_qMVLX-hXbLtdBit55MNFE2mgjaOWUWAbk0RVpQrdGXSAzxXnvJ8t3NM6QHw22JEN1L_fCE1C369SkBPsAlRffB8eW8IJrw05KiuEou1_vvDGtEJnbLwX1Orps8mkJQVQ-ZM6BVw7RSvcFFAs3syJC3iO8smoCzr3qK9ACRjA7azFlBNONcbKyqbOczr0k86QWPPuYY7CjAXg73OszTc4SVhs4_oxaKnoln7pNO2VB0NjFbn2mrcrRFc_hJlLnilmo4Uf0kiZUD-bXOni6wTd1ZJKGXVsu3cDKGrwRcMLtUcLCmo-bCFQP5tc6eLrBN3VkkoZdWy7dwMoavBFwwu1RwsKaj5sICvra712P-6jgiuZixbofwrWQdF_3NrtGO6QFRbq7YB4K-trvXY_7qOCK5mLFuh_CtZB0X_c2u0Y7pAVFurtgHlQP5tc6eLrBN3VkkoZdWy7dwMoavBFwwu1RwsKaj5sICvra712P-6jgiuZixbofwrWQdF_3NrtGO6QFRbq7YB4K-trvXY_7qOCK5mLFuh_CtZB0X_c2u0Y7pAVFurtgHgr62u9dj_uo4IrmYsW6H8K1kHRf9za7RjukBUW6u2AeYUx4jiPSsMEUawCLAAqtsBaRY7_AujfoOeDybUPZNA5hTHiOI9KwwRRrAIsACq2wFpFjv8C6N-g54PJtQ9k0DmFMeI4j0rDBFGsAiwAKrbAWkWO_wLo36Dng8m1D2TQOYUx4jiPSsMEUawCLAAqtsBaRY7_AujfoOeDybUPZNA5hTHiOI9KwwRRrAIsACq2wFpFjv8C6N-g54PJtQ9k0DgEFAAAAAAAAAGFiY2RlAABhYoKkYWGLAAIDBAUGBwgKDQ9hYhBhY4UAAQIDBGFkBaRhYYcCAwQFBgcIYWIJYWOFAAECAwRhZAU"^^<https://w3id.org/security#multibase> _:c14n13 .
        _:c14n12 <http://example.org/vocab/isPatientOf> _:c14n9 _:c14n5 .
        _:c14n12 <http://schema.org/worksFor> _:c14n7 _:c14n5 .
        _:c14n12 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> _:c14n5 .
        _:c14n14 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> _:c14n5 .
        _:c14n14 <https://w3id.org/security#proof> _:c14n10 _:c14n5 .
        _:c14n14 <https://www.w3.org/2018/credentials#credentialSubject> _:c14n12 _:c14n5 .
        _:c14n14 <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n5 .
        _:c14n14 <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n5 .
        _:c14n14 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> _:c14n5 .
        _:c14n2 <http://purl.org/dc/terms/created> "2023-02-03T09:49:25Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n0 .
        _:c14n2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> _:c14n0 .
        _:c14n2 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" _:c14n0 .
        _:c14n2 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> _:c14n0 .
        _:c14n2 <https://w3id.org/security#verificationMethod> <did:example:issuer3#bls12_381-g2-pub001> _:c14n0 .
        _:c14n3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiablePresentation> .
        _:c14n3 <https://w3id.org/security#proof> _:c14n13 .
        _:c14n3 <https://www.w3.org/2018/credentials#verifiableCredential> _:c14n5 .
        _:c14n3 <https://www.w3.org/2018/credentials#verifiableCredential> _:c14n6 .
        _:c14n4 <http://schema.org/status> "active" _:c14n6 .
        _:c14n4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccine> _:c14n6 .
        _:c14n7 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> _:c14n5 .
        _:c14n8 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> _:c14n6 .
        _:c14n8 <https://w3id.org/security#proof> _:c14n0 _:c14n6 .
        _:c14n8 <https://www.w3.org/2018/credentials#credentialSubject> _:c14n4 _:c14n6 .
        _:c14n8 <https://www.w3.org/2018/credentials#expirationDate> "2023-12-31T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n6 .
        _:c14n8 <https://www.w3.org/2018/credentials#issuanceDate> "2020-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> _:c14n6 .
        _:c14n8 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer3> _:c14n6 .
        _:c14n9 <http://example.org/vocab/vaccine> _:c14n4 _:c14n5 .
        _:c14n9 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> _:c14n5 .
        "#;

    #[test]
    fn derive_and_verify_proof_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH).unwrap().into();

        let vc_doc_1 = get_graph_from_ntriples(VC_1).unwrap();
        let vc_proof_1 = get_graph_from_ntriples(VC_PROOF_1).unwrap();
        let vc_1 = VerifiableCredential::new(vc_doc_1, vc_proof_1);

        let disclosed_vc_doc_1 = get_graph_from_ntriples(DISCLOSED_VC_1).unwrap();
        let disclosed_vc_proof_1 = get_graph_from_ntriples(DISCLOSED_VC_PROOF_1).unwrap();
        let disclosed_1 = VerifiableCredential::new(disclosed_vc_doc_1, disclosed_vc_proof_1);

        let vc_doc_2 = get_graph_from_ntriples(VC_2).unwrap();
        let vc_proof_2 = get_graph_from_ntriples(VC_PROOF_2).unwrap();
        let vc_2 = VerifiableCredential::new(vc_doc_2, vc_proof_2);

        let disclosed_vc_doc_2 = get_graph_from_ntriples(DISCLOSED_VC_2).unwrap();
        let disclosed_vc_proof_2 = get_graph_from_ntriples(DISCLOSED_VC_PROOF_2).unwrap();
        let disclosed_2 = VerifiableCredential::new(disclosed_vc_doc_2, disclosed_vc_proof_2);

        let vc_with_disclosed_1 = VcPair::new(vc_1, disclosed_1);
        let vc_with_disclosed_2 = VcPair::new(vc_2, disclosed_2);
        let vcs = vec![vc_with_disclosed_1, vc_with_disclosed_2];

        let deanon_map = get_example_deanon_map();

        let challenge = "abcde";

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            Some(challenge),
            None,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        println!("derived_proof.vp: {}", rdf_canon::serialize(&derived_proof));

        let verified = verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            Some(challenge),
            None,
            HashMap::new(),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_string_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let vc_pairs = vec![
            VcPairString::new(VC_1, VC_PROOF_1, DISCLOSED_VC_1, DISCLOSED_VC_PROOF_1),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        println!("derived_proof: {}", derived_proof);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn verify_proof_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH).unwrap().into();
        let vp = get_dataset_from_nquads(VP).unwrap();
        let challenge = "abcde";
        let verified = verify_proof(
            &mut rng,
            &vp,
            &key_graph,
            Some(challenge),
            None,
            HashMap::new(),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn verify_proof_string_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let challenge = "abcde";
        let verified =
            verify_proof_string(&mut rng, VP, KEY_GRAPH, Some(challenge), None, None, None);
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_with_challenge_and_domain() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH).unwrap().into();

        let vc_doc_1 = get_graph_from_ntriples(VC_1).unwrap();
        let vc_proof_1 = get_graph_from_ntriples(VC_PROOF_1).unwrap();
        let vc_1 = VerifiableCredential::new(vc_doc_1, vc_proof_1);

        let disclosed_vc_doc_1 = get_graph_from_ntriples(DISCLOSED_VC_1).unwrap();
        let disclosed_vc_proof_1 = get_graph_from_ntriples(DISCLOSED_VC_PROOF_1).unwrap();
        let disclosed_1 = VerifiableCredential::new(disclosed_vc_doc_1, disclosed_vc_proof_1);

        let vc_doc_2 = get_graph_from_ntriples(VC_2).unwrap();
        let vc_proof_2 = get_graph_from_ntriples(VC_PROOF_2).unwrap();
        let vc_2 = VerifiableCredential::new(vc_doc_2, vc_proof_2);

        let disclosed_vc_doc_2 = get_graph_from_ntriples(DISCLOSED_VC_2).unwrap();
        let disclosed_vc_proof_2 = get_graph_from_ntriples(DISCLOSED_VC_PROOF_2).unwrap();
        let disclosed_2 = VerifiableCredential::new(disclosed_vc_doc_2, disclosed_vc_proof_2);

        let vc_with_disclosed_1 = VcPair::new(vc_1, disclosed_1);
        let vc_with_disclosed_2 = VcPair::new(vc_2, disclosed_2);
        let vcs = vec![vc_with_disclosed_1, vc_with_disclosed_2];

        let deanon_map = get_example_deanon_map();

        let challenge = Some("challenge");
        let domain = Some("example.org");

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            challenge,
            domain,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        assert!(verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            challenge,
            domain,
            HashMap::new(),
            None
        )
        .is_ok());
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                None,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingDomainInRequest)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                None,
                HashMap::new(),
                None
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            None,
            domain,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            None,
            domain,
            HashMap::new(),
            None,
        )
        .is_ok());
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                None,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                None,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingDomainInRequest)
        ));

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            challenge,
            None,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingDomainInVP)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));
        assert!(verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            challenge,
            None,
            HashMap::new(),
            None,
        )
        .is_ok());
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                None,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            None,
            None,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                None,
                domain,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingDomainInVP)
        ));
        assert!(matches!(
            verify_proof(
                &mut rng,
                &derived_proof,
                &key_graph,
                challenge,
                None,
                HashMap::new(),
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            None,
            None,
            HashMap::new(),
            None,
        )
        .is_ok());
    }

    #[test]
    fn derive_and_verify_proof_string_with_challenge_and_domain() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let vc_pairs = vec![
            VcPairString::new(VC_1, VC_PROOF_1, DISCLOSED_VC_1, DISCLOSED_VC_PROOF_1),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = Some("challenge");
        let domain = Some("example.org");

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            challenge,
            domain,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            challenge,
            domain,
            None,
            None,
        )
        .is_ok());
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                None,
                domain,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                None,
                None,
                None,
            ),
            Err(RDFProofsError::MissingDomainInRequest)
        ));
        assert!(matches!(
            verify_proof_string(&mut rng, &derived_proof, KEY_GRAPH, None, None, None, None,),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            domain,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                domain,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            domain,
            None,
            None,
        )
        .is_ok());
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                None,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(matches!(
            verify_proof_string(&mut rng, &derived_proof, KEY_GRAPH, None, None, None, None,),
            Err(RDFProofsError::MissingDomainInRequest)
        ));

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            challenge,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                domain,
                None,
                None
            ),
            Err(RDFProofsError::MissingDomainInVP)
        ));
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                None,
                domain,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));
        assert!(verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            challenge,
            None,
            None,
            None,
        )
        .is_ok());
        assert!(matches!(
            verify_proof_string(&mut rng, &derived_proof, KEY_GRAPH, None, None, None, None,),
            Err(RDFProofsError::MissingChallengeInRequest)
        ));

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                domain,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                None,
                domain,
                None,
                None,
            ),
            Err(RDFProofsError::MissingDomainInVP)
        ));
        assert!(matches!(
            verify_proof_string(
                &mut rng,
                &derived_proof,
                KEY_GRAPH,
                challenge,
                None,
                None,
                None,
            ),
            Err(RDFProofsError::MissingChallengeInVP)
        ));
        assert!(
            verify_proof_string(&mut rng, &derived_proof, KEY_GRAPH, None, None, None, None,)
                .is_ok()
        );
    }

    const DISCLOSED_VC_1_WITH_HIDDEN_LITERALS: &str = r#"
        _:e0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        _:e0 <http://schema.org/name> _:e4 .
        _:e0 <http://example.org/vocab/isPatientOf> _:b0 .
        _:e0 <http://schema.org/worksFor> _:b1 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/vaccine> _:e1 .
        _:b0 <http://example.org/vocab/vaccinationDate> _:e5 .
        _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> .
        _:e2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        _:e2 <https://www.w3.org/2018/credentials#credentialSubject> _:e0 .
        _:e2 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        _:e2 <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:e2 <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const DEANON_MAP_WITH_HIDDEN_LITERAL: [(&str, &str); 2] = [
        ("_:e4", "\"John Smith\""),
        (
            "_:e5",
            "\"2022-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>",
        ),
    ];
    fn get_example_deanon_map_string_with_hidden_literal() -> HashMap<String, String> {
        DEANON_MAP_WITH_HIDDEN_LITERAL
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn get_example_deanon_map_with_hidden_literal() -> HashMap<NamedOrBlankNode, Term> {
        get_deanon_map_from_string(&&get_example_deanon_map_string_with_hidden_literal()).unwrap()
    }

    #[test]
    fn derive_and_verify_proof_with_hidden_literals() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH).unwrap().into();

        let mut deanon_map = get_example_deanon_map();
        deanon_map.extend(get_example_deanon_map_with_hidden_literal());

        let vc_doc_1 = get_graph_from_ntriples(VC_1).unwrap();
        let vc_proof_1 = get_graph_from_ntriples(VC_PROOF_1).unwrap();
        let vc_1 = VerifiableCredential::new(vc_doc_1, vc_proof_1);

        let disclosed_vc_doc_1 =
            get_graph_from_ntriples(DISCLOSED_VC_1_WITH_HIDDEN_LITERALS).unwrap();
        let disclosed_vc_proof_1 = get_graph_from_ntriples(DISCLOSED_VC_PROOF_1).unwrap();
        let disclosed_1 = VerifiableCredential::new(disclosed_vc_doc_1, disclosed_vc_proof_1);

        let vc_with_disclosed_1 = VcPair::new(vc_1, disclosed_1);
        let vcs = vec![vc_with_disclosed_1];

        let challenge = "abcde";

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            Some(challenge),
            None,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        )
        .unwrap();
        println!("derived_proof: {}", rdf_canon::serialize(&derived_proof));

        let verified = verify_proof(
            &mut rng,
            &derived_proof,
            &key_graph,
            Some(challenge),
            None,
            HashMap::new(),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_string_with_hidden_literals() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let vc_pairs = vec![VcPairString::new(
            VC_1,
            VC_PROOF_1,
            DISCLOSED_VC_1,
            DISCLOSED_VC_PROOF_1,
        )];

        let mut deanon_map = get_example_deanon_map_string();
        deanon_map.extend(get_example_deanon_map_string_with_hidden_literal());

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );

        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_proof_failed_invalid_vc() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed
        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH).unwrap().into();

        let vc_ntriples = r#"
    <did:example:john> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
    <did:example:john> <http://schema.org/name> "**********************************" .  # modified
    <did:example:john> <http://example.org/vocab/isPatientOf> _:b0 .
    <did:example:john> <http://schema.org/worksFor> _:b1 .
    _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
    _:b0 <http://example.org/vocab/lotNumber> "0000001" .
    _:b0 <http://example.org/vocab/vaccinationDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/a> .
    _:b0 <http://example.org/vocab/vaccine> <http://example.org/vaccine/b> .
    _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Organization> .
    _:b1 <http://schema.org/name> "ABC inc." .
    <http://example.org/vcred/00> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#credentialSubject> <did:example:john> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    "#;

        let vc_doc = get_graph_from_ntriples(vc_ntriples).unwrap();
        let vc_proof = get_graph_from_ntriples(VC_PROOF_1).unwrap();
        let vc = VerifiableCredential::new(vc_doc, vc_proof);

        let disclosed_vc_doc = get_graph_from_ntriples(DISCLOSED_VC_1).unwrap();
        let disclosed_vc_proof = get_graph_from_ntriples(DISCLOSED_VC_PROOF_1).unwrap();
        let disclosed = VerifiableCredential::new(disclosed_vc_doc, disclosed_vc_proof);

        let vc_with_disclosed = VcPair::new(vc, disclosed);
        let vcs = vec![vc_with_disclosed];

        let deanon_map = get_example_deanon_map();

        let challenge = "abcde";

        let derived_proof = derive_proof(
            &mut rng,
            &vcs,
            &deanon_map,
            &key_graph,
            Some(challenge),
            None,
            None,
            None,
            None,
            vec![],
            HashMap::new(),
            None,
        );
        assert!(matches!(
            derived_proof,
            Err(RDFProofsError::BBSPlus(
                bbs_plus::prelude::BBSPlusError::InvalidSignature
            ))
        ))
    }

    #[test]
    fn derive_proof_string_failed_invalid_vc() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let vc_pairs = vec![VcPairString::new(
            VC_1_MODIFIED,
            VC_PROOF_1,
            DISCLOSED_VC_1,
            DISCLOSED_VC_PROOF_1,
        )];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(matches!(
            derived_proof,
            Err(RDFProofsError::BBSPlus(
                bbs_plus::prelude::BBSPlusError::InvalidSignature
            ))
        ))
    }

    const VC_PROOF_BOUND_1: &str = r#"
        _:b0 <https://w3id.org/security#proofValue> "utXwiR3cqE_vytaKRk1jO5bijPewZ8Vx67WqHBjJ1TAN8BoEnhdu7zXyZ1WTYuLHqAWQCF5cBR1F0h3FXGsm2xh7Fafg49VG-Slte0XnTgDzpRqn0nqhO4I57s-b3TPVbA_t5uyJnGllyB6QcwVtRQA"^^<https://w3id.org/security#multibase> .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-bound-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const DISCLOSED_VC_PROOF_BOUND_1: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-bound-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;

    #[test]
    fn derive_and_verify_proof_string_with_secret_success() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret = b"SECRET";

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_string_with_invalid_secret_failure() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret = b"INVALID";

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(matches!(
            derived_proof,
            Err(RDFProofsError::BBSPlus(
                bbs_plus::prelude::BBSPlusError::InvalidSignature
            ))
        ))
    }

    #[test]
    fn derive_and_verify_proof_string_without_secret_failure() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(matches!(derived_proof, Err(RDFProofsError::MissingSecret)))
    }

    const VC_PROOF_WITHOUT_PROOFVALUE_1: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const VC_3: &str = r#"
        <did:example:john> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        <did:example:john> <http://schema.org/address> "Somewhere" .
        <http://example.org/vcred/10> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        <http://example.org/vcred/10> <https://www.w3.org/2018/credentials#credentialSubject> <did:example:john> .
        <http://example.org/vcred/10> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer1> .
        <http://example.org/vcred/10> <https://www.w3.org/2018/credentials#issuanceDate> "2022-07-08T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        <http://example.org/vcred/10> <https://www.w3.org/2018/credentials#expirationDate> "2025-07-08T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const VC_PROOF_WITHOUT_PROOFVALUE_3: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <http://purl.org/dc/terms/created> "2023-07-08T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer1#bls12_381-g2-pub001> .
        "#;
    const DISCLOSED_VC_3: &str = r#"
        _:e0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        _:e0 <http://schema.org/address> "Somewhere" .
        _:e9 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        _:e9 <https://www.w3.org/2018/credentials#credentialSubject> _:e0 .
        _:e9 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer1> .
        _:e9 <https://www.w3.org/2018/credentials#issuanceDate> "2022-07-08T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:e9 <https://www.w3.org/2018/credentials#expirationDate> "2025-07-08T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const DISCLOSED_VC_PROOF_BOUND_3: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-bound-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-07-08T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer1#bls12_381-g2-pub001> .
        "#;

    #[test]
    fn derive_and_verify_two_bound_credentials_success() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let secret = b"SECRET";

        let challenge1 = "challenge1";
        let request1 = request_blind_sign_string(&mut rng, secret, Some(challenge1), None).unwrap();
        let verified1 = verify_blind_sign_request_string(
            &mut rng,
            &request1.commitment,
            &request1.pok_for_commitment.unwrap(),
            Some(challenge1),
        );
        assert!(verified1.is_ok());

        let blinded_proof1 = blind_sign_string(
            &mut rng,
            &request1.commitment,
            VC_1,
            VC_PROOF_WITHOUT_PROOFVALUE_1,
            KEY_GRAPH,
        )
        .unwrap();
        let proof1 = unblind_string(VC_1, &blinded_proof1, &request1.blinding).unwrap();
        let result1 = blind_verify_string(secret, VC_1, &proof1, KEY_GRAPH);
        assert!(result1.is_ok(), "{:?}", result1);

        let challenge3 = "challenge3";
        let request3 = request_blind_sign_string(&mut rng, secret, Some(challenge3), None).unwrap();
        let verified3 = verify_blind_sign_request_string(
            &mut rng,
            &request3.commitment,
            &request3.pok_for_commitment.unwrap(),
            Some(challenge3),
        );
        assert!(verified3.is_ok());
        let blinded_proof3 = blind_sign_string(
            &mut rng,
            &request3.commitment,
            VC_3,
            VC_PROOF_WITHOUT_PROOFVALUE_3,
            KEY_GRAPH,
        )
        .unwrap();
        let proof3 = unblind_string(VC_3, &blinded_proof3, &request3.blinding).unwrap();
        let result3 = blind_verify_string(secret, VC_3, &proof3, KEY_GRAPH);
        assert!(result3.is_ok(), "{:?}", result3);

        let vc_pairs = vec![
            VcPairString::new(VC_1, &proof1, DISCLOSED_VC_1, DISCLOSED_VC_PROOF_BOUND_1),
            VcPairString::new(VC_3, &proof3, DISCLOSED_VC_3, DISCLOSED_VC_PROOF_BOUND_3),
        ];

        let mut deanon_map = get_example_deanon_map_string();
        deanon_map.insert(
            "_:e9".to_string(),
            "<http://example.org/vcred/10>".to_string(),
        );

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        println!("derived_proof: {}", derived_proof);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_two_bound_credentials_with_different_secrets_failure() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret1 = b"SECRET1";
        let challenge1 = "challenge1";
        let request1 =
            request_blind_sign_string(&mut rng, secret1, Some(challenge1), None).unwrap();
        let verified1 = verify_blind_sign_request_string(
            &mut rng,
            &request1.commitment,
            &request1.pok_for_commitment.unwrap(),
            Some(challenge1),
        );
        assert!(verified1.is_ok());

        let blinded_proof1 = blind_sign_string(
            &mut rng,
            &request1.commitment,
            VC_1,
            VC_PROOF_WITHOUT_PROOFVALUE_1,
            KEY_GRAPH,
        )
        .unwrap();
        let proof1 = unblind_string(VC_1, &blinded_proof1, &request1.blinding).unwrap();
        let result1 = blind_verify_string(secret1, VC_1, &proof1, KEY_GRAPH);
        assert!(result1.is_ok(), "{:?}", result1);

        let secret3 = b"SECRET3";
        let challenge3 = "challenge3";
        let request3 =
            request_blind_sign_string(&mut rng, secret3, Some(challenge3), None).unwrap();
        let verified3 = verify_blind_sign_request_string(
            &mut rng,
            &request3.commitment,
            &request3.pok_for_commitment.unwrap(),
            Some(challenge3),
        );
        assert!(verified3.is_ok());
        let blinded_proof3 = blind_sign_string(
            &mut rng,
            &request3.commitment,
            VC_3,
            VC_PROOF_WITHOUT_PROOFVALUE_3,
            KEY_GRAPH,
        )
        .unwrap();
        let proof3 = unblind_string(VC_3, &blinded_proof3, &request3.blinding).unwrap();
        let result3 = blind_verify_string(secret3, VC_3, &proof3, KEY_GRAPH);
        assert!(result3.is_ok(), "{:?}", result3);

        let vc_pairs = vec![
            VcPairString::new(VC_1, &proof1, DISCLOSED_VC_1, DISCLOSED_VC_PROOF_BOUND_1),
            VcPairString::new(VC_3, &proof3, DISCLOSED_VC_3, DISCLOSED_VC_PROOF_BOUND_3),
        ];

        let mut deanon_map = get_example_deanon_map_string();
        deanon_map.insert(
            "_:e9".to_string(),
            "<http://example.org/vcred/10>".to_string(),
        );

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret1),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(derived_proof.is_err(), "{:?}", derived_proof)
    }

    #[test]
    fn derive_and_verify_proof_with_commitment_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let secret = b"SECRET";

        let challenge1 = "challenge1";
        let request1 = request_blind_sign_string(&mut rng, secret, Some(challenge1), None).unwrap();
        let verified1 = verify_blind_sign_request_string(
            &mut rng,
            &request1.commitment,
            &request1.pok_for_commitment.unwrap(),
            Some(challenge1),
        );
        assert!(verified1.is_ok());

        let blinded_proof1 = blind_sign_string(
            &mut rng,
            &request1.commitment,
            VC_1,
            VC_PROOF_WITHOUT_PROOFVALUE_1,
            KEY_GRAPH,
        )
        .unwrap();
        let proof1 = unblind_string(VC_1, &blinded_proof1, &request1.blinding).unwrap();
        let result1 = blind_verify_string(secret, VC_1, &proof1, KEY_GRAPH);
        assert!(result1.is_ok(), "{:?}", result1);

        let vc_pairs = vec![VcPairString::new(
            VC_1,
            &proof1,
            DISCLOSED_VC_1,
            DISCLOSED_VC_PROOF_BOUND_1,
        )];

        let deanon_map = get_example_deanon_map_string();

        let blind_sign_request =
            request_blind_sign_string(&mut rng, secret, None, Some(true)).unwrap();

        let challenge = "abcde";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            Some(blind_sign_request),
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_with_only_blind_sign_request_success() {
        let mut rng = StdRng::seed_from_u64(0u64); // TODO: to be fixed

        let secret = b"SECRET";
        let blind_sign_request =
            request_blind_sign_string(&mut rng, secret, None, Some(true)).unwrap();

        let challenge = "abcde";
        let derived_proof = derive_proof_string(
            &mut rng,
            &vec![],
            &HashMap::new(),
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            Some(blind_sign_request),
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_with_ppid_success() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret = b"SECRET";

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";
        let domain = "example.org";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            Some(domain),
            Some(secret),
            None,
            Some(true),
            None,
            None,
            None,
        )
        .unwrap();
        println!("derived_proof:\n{}", derived_proof);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            Some(domain),
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_proof_with_ppid_and_secret_commitment_success() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret = b"SECRET";

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let blind_sign_request =
            request_blind_sign_string(&mut rng, secret, None, Some(true)).unwrap();

        let challenge = "abcde";
        let domain = "example.org";

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            Some(domain),
            Some(secret),
            Some(blind_sign_request),
            Some(true),
            None,
            None,
            None,
        )
        .unwrap();

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            Some(domain),
            None,
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn derive_and_verify_revocable_secret() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let secret = b"SECRET";

        let vc_pairs = vec![
            VcPairString::new(
                VC_1,
                VC_PROOF_BOUND_1,
                DISCLOSED_VC_1,
                DISCLOSED_VC_PROOF_BOUND_1,
            ),
            VcPairString::new(VC_2, VC_PROOF_2, DISCLOSED_VC_2, DISCLOSED_VC_PROOF_2),
        ];

        let deanon_map = get_example_deanon_map_string();

        let challenge = "abcde";

        let (opener_pub_key, _) = elliptic_elgamal_keygen(&mut rng).unwrap();

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            Some(challenge),
            None,
            Some(secret),
            None,
            None,
            None,
            None,
            Some(opener_pub_key),
        )
        .unwrap();
        print!("derived_proof: {}", derived_proof);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            Some(challenge),
            None,
            None,
            Some(opener_pub_key),
        );
        assert!(verified.is_ok(), "{:?}", verified)
    }

    #[test]
    fn generate_circuits() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let circuit_id = "https://zkp-ld.org/circuit/lessThanPrvPrv".to_string();
        let circuit_r1cs = R1CS::from_file("circom/bls12381/less_than_prv_prv_64.r1cs").unwrap();
        let circuit_wasm = std::fs::read("circom/bls12381/less_than_prv_prv_64.wasm").unwrap();
        let commit_witness_count = 2;

        let snark_proving_key = CircomCircuit::setup(circuit_r1cs.clone())
            .generate_proving_key(commit_witness_count, &mut rng)
            .unwrap();

        // serialize
        let circuit_r1cs = ark_to_base64url(&circuit_r1cs).unwrap();
        let circuit_wasm = multibase::encode(Base::Base64Url, circuit_wasm);
        let snark_proving_key = ark_to_base64url(&snark_proving_key).unwrap();

        // TODO: serde_json
        let circuit_json = format!(
            r#"{{
      "id": "{circuit_id}",
      "r1cs": "{circuit_r1cs}",
      "wasm": "{circuit_wasm}",
      "provingKey": "{snark_proving_key}"
    }}"#
        );
        println!("{}", circuit_json);
    }

    #[test]
    fn derive_and_verify_proof_with_less_than_predicates_datetime() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let vc_pairs = vec![VcPairString::new(
            VC_1,
            VC_PROOF_1,
            DISCLOSED_VC_1_WITH_HIDDEN_LITERALS,
            DISCLOSED_VC_PROOF_1,
        )];

        let mut deanon_map = get_example_deanon_map_string();
        deanon_map.extend(get_example_deanon_map_string_with_hidden_literal());

        // define predicates
        let predicates = vec![
                r#"
                _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
                _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
                _:b0 <https://zkp-ld.org/security#private> _:b1 .
                _:b0 <https://zkp-ld.org/security#public> _:b3 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
                _:b2 <https://zkp-ld.org/security#var> "lesser" .
                _:b2 <https://zkp-ld.org/security#val> _:e5 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
                _:b4 <https://zkp-ld.org/security#var> "greater" .
                _:b4 <https://zkp-ld.org/security#val> "2022-12-31T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
                "#.to_string(),
            ];

        // define circuit
        let circuit_r1cs = R1CS::from_file("circom/bls12381/less_than_prv_pub_64.r1cs").unwrap();
        let circuit_wasm = std::fs::read("circom/bls12381/less_than_prv_pub_64.wasm").unwrap();
        let commit_witness_count = 1;
        let snark_proving_key = CircomCircuit::setup(circuit_r1cs.clone())
            .generate_proving_key(commit_witness_count, &mut rng)
            .unwrap();

        // serialize to multibase
        let circuit_r1cs = ark_to_base64url(&circuit_r1cs).unwrap();
        let circuit_wasm = multibase::encode(Base::Base64Url, circuit_wasm);
        let snark_proving_key = ark_to_base64url(&snark_proving_key).unwrap();

        // generate SNARK proving key (by Verifier)
        let circuit = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            CircuitString {
                circuit_r1cs: circuit_r1cs.clone(),
                circuit_wasm: circuit_wasm.clone(),
                snark_proving_key: snark_proving_key.clone(),
            },
        )]);

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);

        let snark_verifying_keys = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            snark_proving_key.clone(),
        )]);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys.clone()),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified);

        // negative test: equality must be rejected
        let predicates_same_datetime = vec![
                r#"
                _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
                _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
                _:b0 <https://zkp-ld.org/security#private> _:b1 .
                _:b0 <https://zkp-ld.org/security#public> _:b3 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
                _:b2 <https://zkp-ld.org/security#var> "lesser" .
                _:b2 <https://zkp-ld.org/security#val> _:e5 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
                _:b4 <https://zkp-ld.org/security#var> "greater" .
                _:b4 <https://zkp-ld.org/security#val> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
                "#.to_string(),
            ];
        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates_same_datetime),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);
        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys),
            None,
        );
        assert!(matches!(
            verified,
            Err(RDFProofsError::ProofSystem(
                proof_system::prelude::ProofSystemError::LegoGroth16Error(_)
            ))
        ));
    }

    #[test]
    fn derive_and_verify_proof_with_less_than_eq_predicates_datetime() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let vc_pairs = vec![VcPairString::new(
            VC_1,
            VC_PROOF_1,
            DISCLOSED_VC_1_WITH_HIDDEN_LITERALS,
            DISCLOSED_VC_PROOF_1,
        )];

        let mut deanon_map = get_example_deanon_map_string();
        deanon_map.extend(get_example_deanon_map_string_with_hidden_literal());

        // define predicates: even the same value is accepted in less-than-eq relationship
        let predicates = vec![
                r#"
                _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
                _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanEqPrvPub> .
                _:b0 <https://zkp-ld.org/security#private> _:b1 .
                _:b0 <https://zkp-ld.org/security#public> _:b3 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
                _:b2 <https://zkp-ld.org/security#var> "lesser" .
                _:b2 <https://zkp-ld.org/security#val> _:e5 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
                _:b4 <https://zkp-ld.org/security#var> "greater" .
                _:b4 <https://zkp-ld.org/security#val> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
                "#.to_string(),
            ];

        // define circuit
        let circuit_r1cs = R1CS::from_file("circom/bls12381/less_than_eq_prv_pub_64.r1cs").unwrap();
        let circuit_wasm = std::fs::read("circom/bls12381/less_than_eq_prv_pub_64.wasm").unwrap();
        let commit_witness_count = 1;
        let snark_proving_key = CircomCircuit::setup(circuit_r1cs.clone())
            .generate_proving_key(commit_witness_count, &mut rng)
            .unwrap();

        // serialize to multibase
        let circuit_r1cs = ark_to_base64url(&circuit_r1cs).unwrap();
        println!("\"r1cs\": \"{}\",", circuit_r1cs);
        let circuit_wasm = multibase::encode(Base::Base64Url, circuit_wasm);
        println!("\"wasm\": \"{}\",", circuit_wasm);
        let snark_proving_key = ark_to_base64url(&snark_proving_key).unwrap();
        println!("\"snarkProvingKey\": \"{}\"", snark_proving_key);

        // generate SNARK proving key (by Verifier)
        let circuit = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanEqPrvPub".to_string(),
            CircuitString {
                circuit_r1cs: circuit_r1cs.clone(),
                circuit_wasm: circuit_wasm.clone(),
                snark_proving_key: snark_proving_key.clone(),
            },
        )]);

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);

        let snark_verifying_keys = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanEqPrvPub".to_string(),
            snark_proving_key.clone(),
        )]);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys.clone()),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified);

        // negative test
        let predicates_lesser_datetime = vec![
                r#"
                _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
                _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanEqPrvPub> .
                _:b0 <https://zkp-ld.org/security#private> _:b1 .
                _:b0 <https://zkp-ld.org/security#public> _:b3 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
                _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
                _:b2 <https://zkp-ld.org/security#var> "lesser" .
                _:b2 <https://zkp-ld.org/security#val> _:e5 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
                _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
                _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
                _:b4 <https://zkp-ld.org/security#var> "greater" .
                _:b4 <https://zkp-ld.org/security#val> "1800-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
                "#.to_string(),
            ];
        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates_lesser_datetime),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);
        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys),
            None,
        );
        assert!(matches!(
            verified,
            Err(RDFProofsError::ProofSystem(
                proof_system::prelude::ProofSystemError::LegoGroth16Error(_)
            ))
        ));
    }

    const VC_4: &str = r#"
        <did:example:john> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        <did:example:john> <http://schema.org/name> "John Smith" .
        <did:example:john> <http://example.org/vocab/isPatientOf> _:b0 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/vaccinationDate> "2022-01-01T00:00:00Z"^^<http://schema.org/DateTime> . # use schema.org instead of xsd
        <http://example.org/vcred/00> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#credentialSubject> <did:example:john> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const VC_PROOF_4: &str = r#"
        _:b0 <https://w3id.org/security#proofValue> "ugsvHVX5633ZzPuy5fKYFyth5Ws6M2mZ8FECcQuDViq_uMM9--yYBtnPdLase-jb_nHL4DdyqBDvkUBbr0eTTUk3vNVI1LRxSfXRqqLng4Qx6SX7tptjtHzjJMkQnolGpiiFfE9k8OhOKcntcJwGSaQ"^^<https://w3id.org/security#multibase> .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const DISCLOSED_VC_4: &str = r#"
        _:e0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Person> .
        _:e0 <http://example.org/vocab/isPatientOf> _:b0 .
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab/Vaccination> .
        _:b0 <http://example.org/vocab/vaccinationDate> _:e1 .
        _:e2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
        _:e2 <https://www.w3.org/2018/credentials#credentialSubject> _:e0 .
        _:e2 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
        _:e2 <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:e2 <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        "#;
    const DISCLOSED_VC_PROOF_4: &str = r#"
        _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
        _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
        _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
        _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
        _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
        "#;
    const DEANON_MAP_4: [(&str, &str); 3] = [
        ("_:e0", "<did:example:john>"),
        (
            "_:e1",
            "\"2022-01-01T00:00:00Z\"^^<http://schema.org/DateTime>",
        ),
        ("_:e2", "<http://example.org/vcred/00>"),
    ];
    fn get_example_deanon_map_4() -> HashMap<String, String> {
        DEANON_MAP_4
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn derive_and_verify_proof_with_less_than_predicates_schema_org_datetime() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let vc_pairs = vec![VcPairString::new(
            VC_4,
            VC_PROOF_4,
            DISCLOSED_VC_4,
            DISCLOSED_VC_PROOF_4,
        )];

        let deanon_map = get_example_deanon_map_4();

        // define predicates
        let predicates = vec![
            r#"
            _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
            _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
            _:b0 <https://zkp-ld.org/security#private> _:b1 .
            _:b0 <https://zkp-ld.org/security#public> _:b3 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
            _:b2 <https://zkp-ld.org/security#var> "lesser" .
            _:b2 <https://zkp-ld.org/security#val> _:e1 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
            _:b4 <https://zkp-ld.org/security#var> "greater" .
            _:b4 <https://zkp-ld.org/security#val> "2022-12-31T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
            "#.to_string(),
        ];

        // define circuit
        let circuit_r1cs = R1CS::from_file("circom/bls12381/less_than_prv_pub_64.r1cs").unwrap();
        let circuit_wasm = std::fs::read("circom/bls12381/less_than_prv_pub_64.wasm").unwrap();
        let commit_witness_count = 1;
        let snark_proving_key = CircomCircuit::setup(circuit_r1cs.clone())
            .generate_proving_key(commit_witness_count, &mut rng)
            .unwrap();

        // serialize to multibase
        let circuit_r1cs = ark_to_base64url(&circuit_r1cs).unwrap();
        println!("\"r1cs\": \"{}\",", circuit_r1cs);
        let circuit_wasm = multibase::encode(Base::Base64Url, circuit_wasm);
        println!("\"wasm\": \"{}\",", circuit_wasm);
        let snark_proving_key = ark_to_base64url(&snark_proving_key).unwrap();
        println!("\"snarkProvingKey\": \"{}\"", snark_proving_key);

        // generate SNARK proving key (by Verifier)
        let circuit = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            CircuitString {
                circuit_r1cs: circuit_r1cs.clone(),
                circuit_wasm: circuit_wasm.clone(),
                snark_proving_key: snark_proving_key.clone(),
            },
        )]);

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);

        let snark_verifying_keys = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            snark_proving_key.clone(),
        )]);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys.clone()),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified);

        // negative test
        let predicates_lesser_datetime = vec![
            r#"
            _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
            _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
            _:b0 <https://zkp-ld.org/security#private> _:b1 .
            _:b0 <https://zkp-ld.org/security#public> _:b3 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
            _:b2 <https://zkp-ld.org/security#var> "lesser" .
            _:b2 <https://zkp-ld.org/security#val> _:e1 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
            _:b4 <https://zkp-ld.org/security#var> "greater" .
            _:b4 <https://zkp-ld.org/security#val> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
            "#.to_string(),
        ];
        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates_lesser_datetime),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);
        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys),
            None,
        );
        assert!(matches!(
            verified,
            Err(RDFProofsError::ProofSystem(
                proof_system::prelude::ProofSystemError::LegoGroth16Error(_)
            ))
        ));
    }

    const VC_5: &str = r#"
    <urn:example:prod1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Product> .
    <urn:example:prod1> <http://schema.org/name> "Awesome Product" .
    <urn:example:prod1> <http://schema.org/price> "300"^^<http://www.w3.org/2001/XMLSchema#integer> .
    <http://example.org/vcred/00> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#credentialSubject> <urn:example:prod1> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    <http://example.org/vcred/00> <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    "#;
    const VC_PROOF_5: &str = r#"
    _:b0 <https://w3id.org/security#proofValue> "upHBxGAvQcU1hUDdvsT8eNvU6g_z9y446mzT78wxCOOToYdDAkX11C-Ga0w_8WNUHnHL4DdyqBDvkUBbr0eTTUk3vNVI1LRxSfXRqqLng4Qx6SX7tptjtHzjJMkQnolGpiiFfE9k8OhOKcntcJwGSaQ"^^<https://w3id.org/security#multibase> .
    _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
    _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
    _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
    _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
    "#;
    const DISCLOSED_VC_5: &str = r#"
    _:e0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/Product> .
    _:e0 <http://schema.org/price> _:e1 .
    _:e2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://www.w3.org/2018/credentials#VerifiableCredential> .
    _:e2 <https://www.w3.org/2018/credentials#credentialSubject> _:e0 .
    _:e2 <https://www.w3.org/2018/credentials#issuer> <did:example:issuer0> .
    _:e2 <https://www.w3.org/2018/credentials#issuanceDate> "2022-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    _:e2 <https://www.w3.org/2018/credentials#expirationDate> "2025-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    "#;
    const DISCLOSED_VC_PROOF_5: &str = r#"
    _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
    _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
    _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
    _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
    "#;
    const DEANON_MAP_5: [(&str, &str); 3] = [
        ("_:e0", "<urn:example:prod1>"),
        (
            "_:e1",
            "\"300\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ),
        ("_:e2", "<http://example.org/vcred/00>"),
    ];
    fn get_example_deanon_map_5() -> HashMap<String, String> {
        DEANON_MAP_5
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn derive_and_verify_proof_with_less_than_predicates_integer() {
        let mut rng = StdRng::seed_from_u64(0u64);

        let vc_pairs = vec![VcPairString::new(
            VC_5,
            VC_PROOF_5,
            DISCLOSED_VC_5,
            DISCLOSED_VC_PROOF_5,
        )];

        let deanon_map = get_example_deanon_map_5();

        // define predicates
        let predicates = vec![
            r#"
            _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
            _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
            _:b0 <https://zkp-ld.org/security#private> _:b1 .
            _:b0 <https://zkp-ld.org/security#public> _:b3 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
            _:b2 <https://zkp-ld.org/security#var> "lesser" .
            _:b2 <https://zkp-ld.org/security#val> _:e1 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
            _:b4 <https://zkp-ld.org/security#var> "greater" .
            _:b4 <https://zkp-ld.org/security#val> "4300000000"^^<http://www.w3.org/2001/XMLSchema#integer> .
            "#.to_string(),
        ];

        // define circuit
        let circuit_r1cs = R1CS::from_file("circom/bls12381/less_than_prv_pub_64.r1cs").unwrap();
        let circuit_wasm = std::fs::read("circom/bls12381/less_than_prv_pub_64.wasm").unwrap();
        let commit_witness_count = 1;
        let snark_proving_key = CircomCircuit::setup(circuit_r1cs.clone())
            .generate_proving_key(commit_witness_count, &mut rng)
            .unwrap();

        // serialize to multibase
        let circuit_r1cs = ark_to_base64url(&circuit_r1cs).unwrap();
        println!("\"r1cs\": \"{}\",", circuit_r1cs);
        let circuit_wasm = multibase::encode(Base::Base64Url, circuit_wasm);
        println!("\"wasm\": \"{}\",", circuit_wasm);
        let snark_proving_key = ark_to_base64url(&snark_proving_key).unwrap();
        println!("\"snarkProvingKey\": \"{}\"", snark_proving_key);

        // generate SNARK proving key (by Verifier)
        let circuit = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            CircuitString {
                circuit_r1cs: circuit_r1cs.clone(),
                circuit_wasm: circuit_wasm.clone(),
                snark_proving_key: snark_proving_key.clone(),
            },
        )]);

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);

        let snark_verifying_keys = HashMap::from([(
            "https://zkp-ld.org/circuit/lessThanPrvPub".to_string(),
            snark_proving_key.clone(),
        )]);

        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys.clone()),
            None,
        );
        assert!(verified.is_ok(), "{:?}", verified);

        // negative test: equality must be rejected
        let predicates_same_integer = vec![
            r#"
            _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#Predicate> .
            _:b0 <https://zkp-ld.org/security#circuit> <https://zkp-ld.org/circuit/lessThanPrvPub> .
            _:b0 <https://zkp-ld.org/security#private> _:b1 .
            _:b0 <https://zkp-ld.org/security#public> _:b3 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b2 .
            _:b1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PrivateVariable> .
            _:b2 <https://zkp-ld.org/security#var> "lesser" .
            _:b2 <https://zkp-ld.org/security#val> _:e1 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> _:b4 .
            _:b3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest> <http://www.w3.org/1999/02/22-rdf-syntax-ns#nil> .
            _:b4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://zkp-ld.org/security#PublicVariable> .
            _:b4 <https://zkp-ld.org/security#var> "greater" .
            _:b4 <https://zkp-ld.org/security#val> "300"^^<http://www.w3.org/2001/XMLSchema#integer> .
            "#.to_string(),
        ];

        let derived_proof = derive_proof_string(
            &mut rng,
            &vc_pairs,
            &deanon_map,
            KEY_GRAPH,
            None,
            None,
            None,
            None,
            None,
            Some(&predicates_same_integer),
            Some(&circuit),
            None,
        )
        .unwrap();
        println!("derive_proof: {}", derived_proof);
        let verified = verify_proof_string(
            &mut rng,
            &derived_proof,
            KEY_GRAPH,
            None,
            None,
            Some(snark_verifying_keys),
            None,
        );
        assert!(matches!(
            verified,
            Err(RDFProofsError::ProofSystem(
                proof_system::prelude::ProofSystemError::LegoGroth16Error(_)
            ))
        ));
    }
}
