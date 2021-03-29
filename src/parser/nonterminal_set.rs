use syn::{Ident, Pat, PatIdent};

use std::cmp::Ordering;
use std::fmt::{self, Display};

#[derive(Clone, Debug)]
pub struct NonterminalSet(pub Pat);

impl NonterminalSet {
    pub fn new_singleton(ident: Ident) -> Self {
        Self(Pat::Ident(PatIdent {
            attrs: vec![],
            by_ref: None,
            mutability: None,
            ident,
            subpat: None,
        }))
    }

    pub fn get_from_singleton(&self) -> Ident {
        self.maybe_get_from_singleton()
            .expect("expected a singleton `NonterminalSet`")
            .clone()
    }

    fn maybe_get_from_singleton(&self) -> Option<&Ident> {
        match &self.0 {
            Pat::Ident(pat_ident) => Some(&pat_ident.ident),
            _ => None,
        }
    }

    fn ordered(&self) -> impl Ord {
        let pat = &self.0;
        quote!(#pat).to_string()
    }
}

impl PartialEq for NonterminalSet {
    fn eq(&self, other: &Self) -> bool {
        self.ordered() == other.ordered()
    }
}

impl Eq for NonterminalSet {}

impl PartialOrd for NonterminalSet {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.ordered().partial_cmp(&other.ordered())
    }
}

impl Ord for NonterminalSet {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ordered().cmp(&other.ordered())
    }
}

impl Display for NonterminalSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.maybe_get_from_singleton() {
            Some(ident) => write!(f, "{}", ident),
            None => write!(f, "{:?}", self),
        }
    }
}
