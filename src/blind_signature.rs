use crate::{
    common::{
        deserialize_ark, get_graph_from_ntriples, get_hasher, get_vc_from_ntriples,
        get_verification_method_identifier, hash_byte_to_field, serialize_ark, BBSPlusSignature,
        Fr, Proof, Statements,
    },
    constants::BLIND_SIG_REQUEST_CONTEXT,
    error::RDFProofsError,
    key_gen::generate_params,
    signature::{configure_proof, hash, transform},
    KeyGraph, VerifiableCredential,
};
use ark_bls12_381::G1Affine;
use ark_serialize::CanonicalSerialize;
use ark_std::{rand::RngCore, UniformRand};
use blake2::Blake2b512;
use multibase::Base;
use oxrdf::Graph;
use proof_system::{
    prelude::MetaStatements,
    proof_spec::ProofSpec,
    statement::ped_comm::PedersenCommitment,
    witness::{Witness, Witnesses},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct BlindSigRequest {
    #[serde(
        rename = "a",
        serialize_with = "serialize_ark",
        deserialize_with = "deserialize_ark"
    )]
    pub commitment: G1Affine,
    #[serde(
        rename = "b",
        serialize_with = "serialize_ark",
        deserialize_with = "deserialize_ark"
    )]
    pub proof: Proof,
}

#[derive(Debug)]
pub struct BlindSigRequestWithBlinding {
    request: BlindSigRequest,
    blinding: Fr,
}

pub fn blind_sig_request<R: RngCore>(
    rng: &mut R,
    secret: &[u8],
    nonce: Option<&str>,
) -> Result<BlindSigRequestWithBlinding, RDFProofsError> {
    // bases := [h_0, h[0]]
    let params = generate_params(1);
    let mut bases = vec![params.h_0];
    bases.push(params.h[0]);

    // blinding to be used in commitment
    let blinding = Fr::rand(rng);

    // secret_int to be committed
    let hasher = get_hasher();
    let secret_int = hash_byte_to_field(secret, &hasher)?;

    // commitment := h_0^{blinding} * h[0]^{secret_int}
    let committed_secret = BTreeMap::from([(0_usize, &secret_int)]);
    let commitment = params.commit_to_messages(committed_secret, &blinding)?;

    // statements := [bases, commitment]
    let mut statements = Statements::new();
    statements.add(PedersenCommitment::new_statement_from_params(
        bases, commitment,
    ));

    // proof_spec := [statements, meta_statements, _, context]
    let context = Some(BLIND_SIG_REQUEST_CONTEXT.to_vec());
    let proof_spec = ProofSpec::new(statements, MetaStatements::new(), vec![], context);
    proof_spec.validate()?;

    // witnesses := [blinding, secret_int]
    let committed_msgs = vec![blinding, secret_int];
    let mut witnesses = Witnesses::new();
    witnesses.add(Witness::PedersenCommitment(committed_msgs));

    // nonce
    let nonce = nonce.map(|v| v.as_bytes().to_vec());

    // proof := NIZK{witnesses: proof_spec}(nonce)
    let proof =
        Proof::new::<R, Blake2b512>(rng, proof_spec, witnesses, nonce, Default::default())?.0;

    Ok(BlindSigRequestWithBlinding {
        request: BlindSigRequest { commitment, proof },
        blinding,
    })
}

pub fn blind_sig_request_string<R: RngCore>(
    rng: &mut R,
    secret: &[u8],
    nonce: Option<&str>,
) -> Result<(String, String), RDFProofsError> {
    let BlindSigRequestWithBlinding { request, blinding } = blind_sig_request(rng, secret, nonce)?;
    let request_cbor = serde_cbor::to_vec(&request)?;
    let request_multibase = multibase::encode(Base::Base64Url, request_cbor);
    let mut blinding_bytes = Vec::new();
    blinding.serialize_compressed(&mut blinding_bytes)?;
    let blinding_base64url = multibase::encode(Base::Base64Url, blinding_bytes);
    Ok((request_multibase, blinding_base64url))
}

pub fn blind_sign<R: RngCore>(
    rng: &mut R,
    request: BlindSigRequest,
    nonce: Option<&str>,
    unsecured_credential: &mut VerifiableCredential,
    key_graph: &KeyGraph,
) -> Result<(), RDFProofsError> {
    let proof_value = blind_sign_core(rng, request, nonce, unsecured_credential, key_graph)?;
    unsecured_credential.add_proof_value(proof_value)?;
    Ok(())
}

pub fn blind_sign_string<R: RngCore>(
    rng: &mut R,
    request_multibase: &str,
    nonce: Option<&str>,
    document: &str,
    proof: &str,
    key_graph: &str,
) -> Result<String, RDFProofsError> {
    let (_, request_cbor) = multibase::decode(request_multibase)?;
    let request = serde_cbor::from_slice(&request_cbor)?;
    let unsecured_credential = get_vc_from_ntriples(document, proof)?;
    let key_graph = get_graph_from_ntriples(key_graph)?.into();
    let proof_value = blind_sign_core(rng, request, nonce, &unsecured_credential, &key_graph)?;
    Ok(proof_value)
}

fn blind_sign_core<R: RngCore>(
    rng: &mut R,
    request: BlindSigRequest,
    nonce: Option<&str>,
    unsecured_credential: &VerifiableCredential,
    key_graph: &KeyGraph,
) -> Result<String, RDFProofsError> {
    verify_blind_sig_request(rng, request.commitment.clone(), request.proof, nonce)?;

    let VerifiableCredential { document, proof } = unsecured_credential;
    let transformed_data = transform(document, proof)?;
    let canonical_proof_config = configure_proof(proof)?;
    let hash_data = hash(&transformed_data, &canonical_proof_config)?;
    let proof_value = serialize_proof_with_comitted_messages(
        rng,
        &request.commitment,
        &hash_data,
        proof,
        key_graph,
    )?;

    Ok(proof_value)
}

fn verify_blind_sig_request<R: RngCore>(
    rng: &mut R,
    commitment: G1Affine,
    proof: Proof,
    nonce: Option<&str>,
) -> Result<(), RDFProofsError> {
    // bases := [h_0, h[0], h[1], ...]
    let params = generate_params(1);
    let mut bases = vec![params.h_0];
    bases.push(params.h[0]);

    // statements := [bases, commitment]
    let mut statements = Statements::new();
    statements.add(PedersenCommitment::new_statement_from_params(
        bases, commitment,
    ));

    // proof_spec := [statements, meta_statements, _, context]
    let context = Some(BLIND_SIG_REQUEST_CONTEXT.to_vec());
    let proof_spec = ProofSpec::new(statements, MetaStatements::new(), vec![], context);
    proof_spec.validate()?;

    // nonce
    let nonce = nonce.map(|v| v.as_bytes().to_vec());

    // verify
    Ok(proof.verify::<R, Blake2b512>(rng, proof_spec, nonce, Default::default())?)
}

fn serialize_proof_with_comitted_messages<R: RngCore>(
    rng: &mut R,
    commitment: &G1Affine,
    hash_data: &Vec<Fr>,
    proof_options: &Graph,
    key_graph: &KeyGraph,
) -> Result<String, RDFProofsError> {
    let _message_count: u32 = hash_data
        .len()
        .try_into()
        .map_err(|_| RDFProofsError::MessageSizeOverflow)?;
    // plus 1 for holder secret
    let message_count = _message_count + 1;

    let uncommitted_messages = hash_data
        .iter()
        .enumerate()
        .map(|(i, m)| (i + 1, m))
        .collect::<BTreeMap<_, _>>();

    let verification_method_identifier = get_verification_method_identifier(proof_options)?;
    let (secret_key, _public_key) = key_graph.get_keypair(verification_method_identifier)?;

    let params = generate_params(message_count);

    let blinded_signature = BBSPlusSignature::new_with_committed_messages(
        rng,
        commitment,
        uncommitted_messages,
        &secret_key,
        &params,
    )?;

    let mut signature_bytes = Vec::new();
    blinded_signature.serialize_compressed(&mut signature_bytes)?;
    let blinded_signature_base64url = multibase::encode(Base::Base64Url, signature_bytes);

    Ok(blinded_signature_base64url)
}

#[cfg(test)]
mod tests {
    use crate::{
        blind_sig_request_string, blind_sign_string, blind_signature::blind_sign,
        common::get_graph_from_ntriples, tests::KEY_GRAPH_NTRIPLES, KeyGraph, VerifiableCredential,
    };

    use super::blind_sig_request;
    use ark_std::rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn blind_sig_request_success() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let secret = b"SECRET";
        let nonce = "NONCE";

        let request = blind_sig_request(&mut rng, secret, Some(nonce));

        assert!(request.is_ok());
        println!("{:#?}", request);
    }

    #[test]
    fn blind_sig_request_string_success() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let secret = b"SECRET";
        let nonce = "NONCE";

        let request = blind_sig_request_string(&mut rng, secret, Some(nonce));

        assert!(request.is_ok());
        println!("{:#?}", request);
    }

    const VC_NTRIPLES_1: &str = r#"
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
    const VC_PROOF_NTRIPLES_WITHOUT_PROOFVALUE_1: &str = r#"
    _:b0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://w3id.org/security#DataIntegrityProof> .
    _:b0 <https://w3id.org/security#cryptosuite> "bbs-termwise-signature-2023" .
    _:b0 <http://purl.org/dc/terms/created> "2023-02-09T09:35:07Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .
    _:b0 <https://w3id.org/security#proofPurpose> <https://w3id.org/security#assertionMethod> .
    _:b0 <https://w3id.org/security#verificationMethod> <did:example:issuer0#bls12_381-g2-pub001> .
    "#;

    #[test]
    fn blind_sign_success() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let secret = b"SECRET";
        let nonce = "NONCE";
        let request = blind_sig_request(&mut rng, secret, Some(nonce)).unwrap();

        let key_graph: KeyGraph = get_graph_from_ntriples(KEY_GRAPH_NTRIPLES).unwrap().into();
        let unsecured_document = get_graph_from_ntriples(VC_NTRIPLES_1).unwrap();
        let proof_config = get_graph_from_ntriples(VC_PROOF_NTRIPLES_WITHOUT_PROOFVALUE_1).unwrap();
        let mut vc = VerifiableCredential::new(unsecured_document, proof_config);
        let result = blind_sign(&mut rng, request.request, Some(nonce), &mut vc, &key_graph);
        assert!(result.is_ok());

        println!("{}", rdf_canon::canonicalize_graph(&vc.document).unwrap());
        println!("{}", rdf_canon::canonicalize_graph(&vc.proof).unwrap());
    }

    #[test]
    fn blind_sign_string_success() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let secret = b"SECRET";
        let nonce = "NONCE";
        let request = blind_sig_request_string(&mut rng, secret, Some(nonce)).unwrap();

        let result = blind_sign_string(
            &mut rng,
            &request.0,
            Some(nonce),
            VC_NTRIPLES_1,
            VC_PROOF_NTRIPLES_WITHOUT_PROOFVALUE_1,
            KEY_GRAPH_NTRIPLES,
        );
        println!("result: {:#?}", result);
        assert!(result.is_ok());
    }
}