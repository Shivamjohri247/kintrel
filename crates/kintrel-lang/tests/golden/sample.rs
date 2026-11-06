//! Module docs stay in the file-level atom.

use std::collections::{HashMap, HashSet};
use std::fmt;
pub use crate::cart::Cart as C;
extern crate serde;

/// A cart line.
#[derive(Debug, Clone)]
pub struct Line {
    pub sku: String,
    pub qty: u32,
}

// Free comment, separated by a blank line: glue.

/// Doc attached to the impl.
impl Line {
    pub const MAX: u32 = 99;

    /// Price in cents.
    pub fn price(&self) -> u64 {
        fn helper() {} // nested item: part of price, not an atom
        self.qty as u64 * 100
    }

    #[inline]
    pub fn price(&self, currency: &str) -> u64 { 0 }
}

impl<T: fmt::Debug> fmt::Display for Wrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

pub trait Priced {
    type Money;
    const ZERO: u64;
    fn price(&self) -> u64;
    fn double(&self) -> u64 { self.price() * 2 }
}

mod inner {
    pub fn f() {}
    mod deeper { pub fn g() {} }
}

mod outer;

macro_rules! square { ($x:expr) => { $x * $x }; }

static COUNT: u32 = 0;
type Map = HashMap<String, u32>;
enum Kind { A, B }
union U { a: u32, b: f32 }

fn main() {
    let _ = square!(2);
}
