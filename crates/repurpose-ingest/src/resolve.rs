//! Name resolution for the fetcher. Every answer is checked by the caller before a
//! connection is made, and the connection goes to the checked address.

#[cfg(feature = "test-support")]
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
#[cfg(feature = "test-support")]
use std::sync::{Arc, Mutex};

/// The most addresses taken from one answer.
pub const MAX_ANSWERS: usize = 16;

#[derive(Debug, Clone, Default)]
pub enum Resolver {
    /// The operating system's resolver (`getaddrinfo` on a blocking thread).
    #[default]
    System,
    /// Canned answers for tests; a name answers each listed set in turn, then repeats
    /// the last. Records every lookup.
    #[cfg(feature = "test-support")]
    Canned(Arc<Canned>),
}

#[cfg(feature = "test-support")]
#[derive(Debug, Default)]
pub struct Canned {
    answers: Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
    lookups: Mutex<Vec<String>>,
}

#[cfg(feature = "test-support")]
impl Canned {
    pub fn new(entries: &[(&str, &[&[IpAddr]])]) -> Arc<Self> {
        let answers = entries
            .iter()
            .map(|(name, sets)| {
                (
                    (*name).to_owned(),
                    sets.iter().map(|s| s.to_vec()).collect(),
                )
            })
            .collect();
        Arc::new(Self {
            answers: Mutex::new(answers),
            lookups: Mutex::default(),
        })
    }

    /// Every name looked up so far, in order.
    pub fn lookups(&self) -> Vec<String> {
        self.lookups.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn answer(&self, name: &str) -> Vec<IpAddr> {
        if let Ok(mut lookups) = self.lookups.lock() {
            lookups.push(name.to_owned());
        }
        let Ok(mut answers) = self.answers.lock() else {
            return Vec::new();
        };
        match answers.get_mut(name) {
            Some(sets) if sets.len() > 1 => sets.remove(0),
            Some(sets) => sets.first().cloned().unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

impl Resolver {
    /// The addresses `name` resolves to (deduplicated, at most [`MAX_ANSWERS`]).
    /// The caller bounds the wait.
    pub async fn lookup(&self, name: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        let mut found: Vec<IpAddr> = match self {
            Self::System => tokio::net::lookup_host((name, port))
                .await?
                .map(|addr| addr.ip())
                .take(MAX_ANSWERS)
                .collect(),
            #[cfg(feature = "test-support")]
            Self::Canned(canned) => canned.answer(name),
        };
        found.dedup();
        found.truncate(MAX_ANSWERS);
        if found.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no addresses"));
        }
        Ok(found)
    }
}
