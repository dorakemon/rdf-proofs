use crate::{
    context::PROOF,
    ordered_triple::{
        OrderedGraphNameRef, OrderedGraphViews, OrderedVerifiableCredentialGraphViews,
    },
};
use oxrdf::{dataset::GraphView, Graph, Triple};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct VerifiableCredential {
    pub document: Graph,
    pub proof: Graph,
}

impl VerifiableCredential {
    pub fn new(document: Graph, proof: Graph) -> Self {
        Self { document, proof }
    }
}

impl std::fmt::Display for VerifiableCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "document:")?;
        for t in self.document.iter() {
            writeln!(f, "{} .", t.to_string())?;
        }
        writeln!(f, "proof:")?;
        for t in self.proof.iter() {
            writeln!(f, "{} .", t.to_string())?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct VerifiableCredentialView<'a> {
    pub document: GraphView<'a>,
    pub proof: GraphView<'a>,
}

impl<'a> VerifiableCredentialView<'a> {
    pub fn new(document: GraphView<'a>, proof: GraphView<'a>) -> Self {
        Self { document, proof }
    }
}

#[derive(Clone)]
pub struct VerifiableCredentialTriples {
    pub document: Vec<Triple>,
    pub proof: Vec<Triple>,
}

impl VerifiableCredentialTriples {
    pub fn new(document: Vec<Triple>, proof: Vec<Triple>) -> Self {
        Self { document, proof }
    }
}

impl From<VerifiableCredentialView<'_>> for VerifiableCredentialTriples {
    fn from(view: VerifiableCredentialView) -> Self {
        let mut document = view
            .document
            .iter()
            .filter(|t| t.predicate != PROOF) // filter out `proof`
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        document.sort_by_cached_key(|t| t.to_string());
        let mut proof = view
            .proof
            .iter()
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        proof.sort_by_cached_key(|t| t.to_string());
        Self { document, proof }
    }
}

impl From<&VerifiableCredentialView<'_>> for VerifiableCredentialTriples {
    fn from(view: &VerifiableCredentialView) -> Self {
        let mut document = view
            .document
            .iter()
            .filter(|t| t.predicate != PROOF) // filter out `proof`
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        document.sort_by_cached_key(|t| t.to_string());
        let mut proof = view
            .proof
            .iter()
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        proof.sort_by_cached_key(|t| t.to_string());
        Self { document, proof }
    }
}

impl From<&VerifiableCredential> for VerifiableCredentialTriples {
    fn from(view: &VerifiableCredential) -> Self {
        let mut document = view
            .document
            .iter()
            .filter(|t| t.predicate != PROOF) // filter out `proof`
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        document.sort_by_cached_key(|t| t.to_string());
        let mut proof = view
            .proof
            .iter()
            .map(|t| t.into_owned())
            .collect::<Vec<_>>();
        proof.sort_by_cached_key(|t| t.to_string());
        Self { document, proof }
    }
}

#[derive(Debug)]
pub struct DisclosedVerifiableCredential {
    pub document: BTreeMap<usize, Option<Triple>>,
    pub proof: BTreeMap<usize, Option<Triple>>,
}

pub struct VcPair {
    pub original: VerifiableCredential,
    pub disclosed: VerifiableCredential,
}

impl VcPair {
    pub fn new(original: VerifiableCredential, disclosed: VerifiableCredential) -> Self {
        Self {
            original,
            disclosed,
        }
    }

    pub fn to_string(&self) -> String {
        format!(
            "vc:\n{}vc_proof:\n{}\ndisclosed_vc:\n{}disclosed_vc_proof:\n{}\n",
            &self
                .original
                .document
                .iter()
                .map(|q| format!("{} .\n", q.to_string()))
                .collect::<String>(),
            &self
                .original
                .proof
                .iter()
                .map(|q| format!("{} .\n", q.to_string()))
                .collect::<String>(),
            &self
                .disclosed
                .document
                .iter()
                .map(|q| format!("{} .\n", q.to_string()))
                .collect::<String>(),
            &self
                .disclosed
                .proof
                .iter()
                .map(|q| format!("{} .\n", q.to_string()))
                .collect::<String>()
        )
    }
}

pub struct VpGraphs<'a> {
    pub metadata: GraphView<'a>,
    pub proof: GraphView<'a>,
    pub proof_graph_name: OrderedGraphNameRef<'a>,
    pub filters: OrderedGraphViews<'a>,
    pub disclosed_vcs: OrderedVerifiableCredentialGraphViews<'a>,
}
