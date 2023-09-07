use crate::{
    common::{
        decompose_vp, get_delimiter, get_hasher, hash_term_to_field, is_nym, reorder_vc_triples,
        Fr, ProofG1, ProofWithIndexMap,
    },
    context::{CHALLENGE, PROOF_VALUE, VERIFICATION_METHOD},
    error::RDFProofsError,
    key_gen::generate_params,
    key_graph::KeyGraph,
    ordered_triple::OrderedNamedOrBlankNode,
    vc::{DisclosedVerifiableCredential, VerifiableCredentialTriples, VpGraphs},
};
use ark_bls12_381::Bls12_381;
use ark_ec::pairing::Pairing;
use ark_serialize::CanonicalDeserialize;
use ark_std::rand::RngCore;
use bbs_plus::prelude::PublicKeyG2 as BBSPublicKeyG2;
use blake2::Blake2b512;
use oxrdf::{
    dataset::GraphView, Dataset, GraphNameRef, NamedOrBlankNode, Subject, Term, TermRef, Triple,
};
use proof_system::{
    prelude::{EqualWitnesses, MetaStatements},
    proof_spec::ProofSpec,
    statement::{bbs_plus::PoKBBSSignatureG1, Statements},
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// verify VP
pub fn verify_proof<R: RngCore>(
    rng: &mut R,
    vp: &Dataset,
    nonce: Option<&str>,
    key_graph: &KeyGraph,
) -> Result<(), RDFProofsError> {
    println!("VP:\n{}", rdf_canon::serialize(&vp));

    // decompose VP into graphs to identify VP proof and proof graph name
    let VpGraphs {
        proof: vp_proof_with_value,
        proof_graph_name,
        ..
    } = decompose_vp(vp)?;
    let proof_graph_name: GraphNameRef = proof_graph_name.into();

    // get proof value
    let proof_value_triple = vp_proof_with_value
        .triples_for_predicate(PROOF_VALUE)
        .next()
        .ok_or(RDFProofsError::InvalidVP)?;
    let proof_value_encoded = match proof_value_triple.object {
        TermRef::Literal(v) => Ok(v.value()),
        _ => Err(RDFProofsError::InvalidVP),
    }?;

    // drop proof value from VP proof before canonicalization
    // (otherwise it could differ from the prover's canonicalization)
    let vp_without_proof_value = Dataset::from_iter(
        vp.iter()
            .filter(|q| !(q.predicate == PROOF_VALUE && q.graph_name == proof_graph_name)),
    );

    // nonce check
    let get_nonce = || {
        let nonce_in_vp_triple = vp_proof_with_value.triples_for_predicate(CHALLENGE).next();
        if let Some(triple) = nonce_in_vp_triple {
            if let TermRef::Literal(v) = triple.object {
                Ok(Some(v.value()))
            } else {
                Err(RDFProofsError::InvalidChallengeDatatype)
            }
        } else {
            Ok(None)
        }
    };
    match (nonce, get_nonce()?) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(RDFProofsError::MissingChallengeInRequest),
        (Some(_), None) => Err(RDFProofsError::MissingChallengeInVP),
        (Some(given_nonce), Some(nonce_in_vp)) => {
            if given_nonce == nonce_in_vp {
                Ok(())
            } else {
                Err(RDFProofsError::MismatchedChallenge)
            }
        }
    }?;

    // canonicalize VP
    let c14n_map_for_disclosed = rdf_canon::issue(&vp_without_proof_value)?;
    let canonicalized_vp = rdf_canon::relabel(&vp_without_proof_value, &c14n_map_for_disclosed)?;
    println!(
        "canonicalized VP:\n{}",
        rdf_canon::serialize(&canonicalized_vp)
    );

    // TODO: check VP

    // decompose canonicalized VP into graphs
    let VpGraphs {
        metadata: _,
        proof: _,
        proof_graph_name: _,
        filters: _filters_graph,
        disclosed_vcs: c14n_disclosed_vc_graphs,
    } = decompose_vp(&canonicalized_vp)?;

    // get issuer public keys
    let public_keys = c14n_disclosed_vc_graphs
        .iter()
        .map(|(_, vc)| get_public_keys_from_graphview(&vc.proof, key_graph))
        .collect::<Result<Vec<_>, _>>()?;
    println!("public_keys:\n{:#?}\n", public_keys);

    // convert to Vecs
    let disclosed_vec = c14n_disclosed_vc_graphs
        .into_iter()
        .map(|(_, v)| v.into())
        .collect::<Vec<VerifiableCredentialTriples>>();

    // deserialize proof value into proof and index_map
    let (_, proof_value_bytes) = multibase::decode(proof_value_encoded)?;
    let ProofWithIndexMap {
        proof: proof_bytes,
        index_map,
    } = serde_cbor::from_slice(&proof_value_bytes)?;
    let proof = ProofG1::deserialize_compressed(&*proof_bytes)?;
    println!("proof:\n{:#?}\n", proof);
    println!("index_map:\n{:#?}\n", index_map);

    // reorder statements according to index map
    let reordered_vc_triples = reorder_vc_triples(&disclosed_vec, &index_map)?;
    println!(
        "reordered_disclosed_vc_triples:\n{:#?}\n",
        reordered_vc_triples
    );

    // identify disclosed terms
    let disclosed_terms = reordered_vc_triples
        .iter()
        .enumerate()
        .map(|(i, disclosed_vc_triples)| get_disclosed_terms(disclosed_vc_triples, i))
        .collect::<Result<Vec<_>, RDFProofsError>>()?;
    println!("disclosed_terms:\n{:#?}\n", disclosed_terms);

    let params_and_pks = disclosed_terms
        .iter()
        .zip(public_keys)
        .map(|(t, pk)| (generate_params(t.term_count), pk));

    // merge each partial equivs
    let mut equivs: BTreeMap<OrderedNamedOrBlankNode, Vec<(usize, usize)>> = BTreeMap::new();
    for DisclosedTerms {
        equivs: partial_equivs,
        ..
    } in &disclosed_terms
    {
        for (k, v) in partial_equivs {
            equivs
                .entry(k.clone().into())
                .or_default()
                .extend(v.clone());
        }
    }
    // drop single-element vecs from equivs
    let equivs: BTreeMap<OrderedNamedOrBlankNode, Vec<(usize, usize)>> =
        equivs.into_iter().filter(|(_, v)| v.len() > 1).collect();

    // build statements
    let mut statements = Statements::<Bls12_381, <Bls12_381 as Pairing>::G1Affine>::new();
    for (DisclosedTerms { disclosed, .. }, (params, public_key)) in
        disclosed_terms.iter().zip(params_and_pks)
    {
        statements.add(PoKBBSSignatureG1::new_statement_from_params(
            params,
            public_key,
            disclosed.clone(),
        ));
    }

    // build meta statements
    let mut meta_statements = MetaStatements::new();
    for (_, equiv_vec) in equivs {
        let equiv_set: BTreeSet<(usize, usize)> = equiv_vec.into_iter().collect();
        meta_statements.add_witness_equality(EqualWitnesses(equiv_set));
    }

    // build context
    let serialized_vp = rdf_canon::serialize(&canonicalized_vp).into_bytes();
    let serialized_vp_with_index_map = ProofWithIndexMap {
        proof: serialized_vp,
        index_map: index_map.clone(),
    };
    let context = serde_cbor::to_vec(&serialized_vp_with_index_map)?;

    // build proof spec
    let proof_spec = ProofSpec::new(statements, meta_statements, vec![], Some(context));
    proof_spec.validate()?;

    // verify proof
    Ok(proof.verify::<R, Blake2b512>(
        rng,
        proof_spec,
        nonce.map(|v| v.as_bytes().to_vec()),
        Default::default(),
    )?)
}

#[derive(Debug)]
struct DisclosedTerms {
    disclosed: BTreeMap<usize, Fr>,
    equivs: HashMap<NamedOrBlankNode, Vec<(usize, usize)>>,
    term_count: usize,
}

fn get_disclosed_terms(
    disclosed_vc_triples: &DisclosedVerifiableCredential,
    vc_index: usize,
) -> Result<DisclosedTerms, RDFProofsError> {
    let mut disclosed_terms = BTreeMap::<usize, Fr>::new();
    let mut equivs = HashMap::<NamedOrBlankNode, Vec<(usize, usize)>>::new();

    let DisclosedVerifiableCredential {
        document: disclosed_document,
        proof: disclosed_proof,
    } = disclosed_vc_triples;

    for (j, disclosed_triple) in disclosed_document {
        let subject_index = 3 * j;
        build_disclosed_terms(
            disclosed_triple,
            subject_index,
            vc_index,
            &mut disclosed_terms,
            &mut equivs,
        )?;
    }

    let delimiter_index = disclosed_document.len() * 3;
    let proof_index = delimiter_index + 1;
    let delimiter = get_delimiter()?;
    disclosed_terms.insert(delimiter_index, delimiter);

    for (j, disclosed_triple) in disclosed_proof {
        let subject_index = 3 * j + proof_index;
        build_disclosed_terms(
            disclosed_triple,
            subject_index,
            vc_index,
            &mut disclosed_terms,
            &mut equivs,
        )?;
    }
    Ok(DisclosedTerms {
        disclosed: disclosed_terms,
        equivs,
        term_count: (disclosed_document.len() + disclosed_proof.len()) * 3 + 1,
    })
}

fn build_disclosed_terms(
    disclosed_triple: &Option<Triple>,
    subject_index: usize,
    vc_index: usize,
    disclosed_terms: &mut BTreeMap<usize, Fr>,
    equivs: &mut HashMap<NamedOrBlankNode, Vec<(usize, usize)>>,
) -> Result<(), RDFProofsError> {
    let predicate_index = subject_index + 1;
    let object_index = subject_index + 2;

    let hasher = get_hasher();

    match disclosed_triple {
        Some(triple) => {
            match &triple.subject {
                Subject::BlankNode(b) => {
                    equivs
                        .entry(NamedOrBlankNode::BlankNode(b.clone().into()))
                        .or_default()
                        .push((vc_index, subject_index));
                }
                Subject::NamedNode(n) if is_nym(n) => {
                    equivs
                        .entry(NamedOrBlankNode::NamedNode(n.clone().into()))
                        .or_default()
                        .push((vc_index, subject_index));
                }
                Subject::NamedNode(n) => {
                    let subject_fr = hash_term_to_field(n.into(), &hasher)?;
                    disclosed_terms.insert(subject_index, subject_fr);
                }
                #[cfg(feature = "rdf-star")]
                Subject::Triple(_) => return Err(RDFProofsError::RDFStarUnsupported),
            };

            if is_nym(&triple.predicate) {
                equivs
                    .entry(NamedOrBlankNode::NamedNode(triple.predicate.clone().into()))
                    .or_default()
                    .push((vc_index, predicate_index));
            } else {
                let predicate_fr = hash_term_to_field((&triple.predicate).into(), &hasher)?;
                disclosed_terms.insert(predicate_index, predicate_fr);
            };

            match &triple.object {
                Term::BlankNode(b) => {
                    equivs
                        .entry(NamedOrBlankNode::BlankNode(b.clone().into()))
                        .or_default()
                        .push((vc_index, object_index));
                }
                Term::NamedNode(n) if is_nym(n) => {
                    equivs
                        .entry(NamedOrBlankNode::NamedNode(n.clone().into()))
                        .or_default()
                        .push((vc_index, object_index));
                }
                Term::NamedNode(n) => {
                    let object_fr = hash_term_to_field(n.into(), &hasher)?;
                    disclosed_terms.insert(object_index, object_fr);
                }
                Term::Literal(v) => {
                    let object_fr = hash_term_to_field(v.into(), &hasher)?;
                    disclosed_terms.insert(object_index, object_fr);
                }
                #[cfg(feature = "rdf-star")]
                Term::Triple(_) => return Err(RDFProofsError::DeriveProofValue),
            };
        }

        None => {}
    };
    Ok(())
}

// TODO: to be integrated with `get_public_keys`
fn get_public_keys_from_graphview(
    proof_graph: &GraphView,
    key_graph: &KeyGraph,
) -> Result<BBSPublicKeyG2<Bls12_381>, RDFProofsError> {
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