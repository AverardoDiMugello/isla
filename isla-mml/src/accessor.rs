// BSD 2-Clause License
//
// Copyright (c) 2022 Alasdair Armstrong
//
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
// 1. Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright
// notice, this list of conditions and the following disclaimer in the
// documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Accessors are paths into the events generated by Isla in a
//! trace. An Isla event may contain arbitrary data provided by the
//! Sail model, so we need some way to access that data from the SMT
//! memory model.

use std::borrow::Borrow;
use std::collections::HashMap;

use isla_lib::bitvector::BV;
use isla_lib::ir::{SharedState, Val};
use isla_lib::smt::smtlib::Ty;
use isla_lib::smt::{Event, Sym};
use isla_lib::zencode;

use crate::memory_model::constants::*;
use crate::memory_model::{Accessor, Name, Symtab};
use crate::smt::{Sexp, SexpArena, SexpId};

/// Because isla-axiomatic imports isla-mml, we don't know the
/// concrete (axiomatic) event type yet. Therefore, we define a trait
/// that the event type must implement.
pub trait ModelEvent<'ev, B> {
    fn name(&self) -> Name;

    fn base_events(&self) -> &[&'ev Event<B>];

    fn base(&self) -> Option<&'ev Event<B>> {
        match self.base_events() {
            &[ev] => Some(ev),
            _ => None,
        }
    }

    fn opcode(&self) -> B;
}

#[derive(Debug)]
pub enum AccessorTree<'a> {
    Node { elem: &'a Accessor, child: Box<AccessorTree<'a>> },
    Match { arms: HashMap<Option<Name>, AccessorTree<'a>> },
    Leaf,
}

static ACCESSORTREE_LEAF: AccessorTree<'static> = AccessorTree::Leaf;

impl<'a> AccessorTree<'a> {
    pub fn from_accessors(accessors: &'a [Accessor]) -> Self {
        let mut constructor_stack = Vec::new();
        let mut cur = AccessorTree::Leaf;

        for accessor in accessors {
            match accessor {
                Accessor::Ctor(ctor) => {
                    constructor_stack.push((Some(*ctor), cur));
                    cur = AccessorTree::Leaf
                }
                Accessor::Wildcard => {
                    constructor_stack.push((None, cur));
                    cur = AccessorTree::Leaf
                }
                Accessor::Match(n) => {
                    let mut arms = constructor_stack.split_off(constructor_stack.len() - n);
                    cur = AccessorTree::Match { arms: arms.drain(..).collect() }
                }
                acc => cur = AccessorTree::Node { elem: acc, child: Box::new(cur) },
            }
        }

        assert!(constructor_stack.is_empty());

        cur
    }
}

#[derive(Copy, Clone, Debug)]
enum AccessorVal<'ev, B> {
    Val(&'ev Val<B>),
    Bits(B),
    Sexp(SexpId),
}

impl<'ev, B: BV> AccessorVal<'ev, B> {
    fn to_sexp(self, sexps: &mut SexpArena) -> Option<SexpId> {
        Some(match self {
            AccessorVal::Sexp(id) => id,
            AccessorVal::Bits(bv) => sexps.alloc(Sexp::Bits(bv.to_vec())),
            AccessorVal::Val(v) => match v {
                Val::Bool(true) => sexps.bool_true,
                Val::Bool(false) => sexps.bool_false,
                Val::Bits(bv) => sexps.alloc(Sexp::Bits(bv.to_vec())),
                Val::Symbolic(v) => sexps.alloc(Sexp::Symbolic(*v)),
                Val::Enum(e) => sexps.alloc(Sexp::Enum(e.member, e.enum_id.to_usize())),
                _ => return None,
            },
        })
    }
}

// This type represents the view into an event as we walk down into it.
struct View<'ev, B> {
    name: Option<String>,
    special: HashMap<String, AccessorVal<'ev, B>>,
    values: Option<&'ev [Val<B>]>,
    value: Option<AccessorVal<'ev, B>>,
}

impl<'ev, B: BV> Default for View<'ev, B> {
    fn default() -> Self {
        View { name: None, special: HashMap::new(), values: None, value: None }
    }
}

macro_rules! access_extension {
    ($id: ident, $smt_extension: ident, $concrete_extension: path) => {
        fn $id(&mut self, n: u32, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) {
            if let Some(len) = self.simplify_to_sexp_or_bits(types, sexps) {
                if n == len {
                    return;
                } else if n < len {
                    *self = Self::default();
                    return;
                }
                match self.value {
                    Some(AccessorVal::Sexp(sexp)) => {
                        let extend_by = sexps.alloc(Sexp::Int(n - len));
                        let extend = sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.$smt_extension, extend_by]));
                        self.set_sexp(sexps.alloc(Sexp::List(vec![extend, sexp])))
                    }
                    Some(AccessorVal::Bits(bv)) => {
                        if n > B::MAX_WIDTH {
                            let extend_by = sexps.alloc(Sexp::Int(n - len));
                            let extend =
                                sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.$smt_extension, extend_by]));
                            let sexp = sexps.alloc(Sexp::Bits(bv.to_vec()));
                            self.set_sexp(sexps.alloc(Sexp::List(vec![extend, sexp])))
                        } else {
                            self.set_bits($concrete_extension(bv, n))
                        }
                    }
                    _ => *self = Self::default(),
                }
            } else {
                *self = Self::default()
            }
        }
    };
}

impl<'ev, B: BV> View<'ev, B> {
    fn new(opcode: B) -> Self {
        let mut view = Self::default();
        view.special.insert("opcode".to_string(), AccessorVal::Bits(opcode));
        view
    }

    fn with_name<S: Into<String>>(mut self, name: S) -> Self {
        self.name = Some(name.into());
        self
    }

    fn with_special<S: Into<String>>(mut self, key: S, value: &'ev Val<B>) -> Self {
        self.special.insert(key.into(), AccessorVal::Val(value));
        self
    }

    fn with_value(mut self, value: &'ev Val<B>) -> Self {
        self.value = Some(AccessorVal::Val(value));
        self
    }

    fn with_values(mut self, values: &'ev [Val<B>]) -> Self {
        match values {
            [value] => self.value = Some(AccessorVal::Val(value)),
            _ => self.values = Some(values),
        }
        self
    }

    fn set_value(&mut self, value: &'ev Val<B>) {
        self.value = Some(AccessorVal::Val(value))
    }

    fn set_accessor_value(&mut self, value: AccessorVal<'ev, B>) {
        self.value = Some(value)
    }

    fn set_sexp(&mut self, sexp: SexpId) {
        self.value = Some(AccessorVal::Sexp(sexp))
    }

    fn set_bits(&mut self, bv: B) {
        self.value = Some(AccessorVal::Bits(bv))
    }

    fn access_tuple(&mut self, n: usize, shared_state: &SharedState<B>) {
        if let Some(values) = self.values {
            if let Some(value) = values.get(n) {
                self.values = None;
                self.set_value(value);
                return;
            }
        } else if let Some(AccessorVal::Val(Val::Struct(fields))) = self.value {
            for (name, field_value) in fields.iter() {
                if shared_state.symtab.tuple_struct_field_number(*name) == Some(n) {
                    self.set_value(field_value);
                    return;
                }
            }
        }
        *self = Self::default()
    }

    fn access_special<S: Into<String>>(&mut self, key: S) {
        if let Some(value) = self.special.get(&key.into()) {
            self.set_accessor_value(*value)
        } else {
            *self = Self::default()
        }
    }

    fn access_match<'a, 'b, 'c>(
        &'a mut self,
        arms: &'b HashMap<Option<Name>, AccessorTree<'c>>,
        symtab: &Symtab,
        shared_state: &SharedState<B>,
    ) -> &'b AccessorTree<'c> {
        if let Some(AccessorVal::Val(Val::Ctor(ctor_name, value))) = self.value {
            let ctor_name = shared_state.symtab.to_str_demangled(*ctor_name);
            self.set_value(value);
            let n = &symtab.lookup(&zencode::decode(ctor_name));
            return match arms.get(n) {
                Some(accessor_tree) => accessor_tree,
                // If the constructor isn't in the match arms, return the wildcard using None
                None => &arms[&None],
            };
        }

        *self = Self::default();
        &ACCESSORTREE_LEAF
    }

    fn access_is_name(&mut self, expected_name: &str) {
        match &self.name {
            Some(name) if name.as_str() == expected_name => self.set_value(&Val::Bool(true)),
            _ => self.set_value(&Val::Bool(false)),
        }
    }

    fn access_literal_id(&mut self, id: Name, sexps: &mut SexpArena) {
        if id == TRUE.name() {
            self.set_sexp(sexps.bool_true)
        } else if id == FALSE.name() {
            self.set_sexp(sexps.bool_false)
        } else if id == DEFAULT.name() {
            *self = Self::default()
        }
    }

    fn access_field(&mut self, field: Name, symtab: &Symtab, shared_state: &SharedState<B>) {
        if let Some(sym) = symtab.get(field) {
            if let Some(AccessorVal::Val(Val::Struct(fields))) = self.value {
                for (field_name, field_value) in fields {
                    if zencode::decode(shared_state.symtab.to_str_demangled(*field_name)) == sym {
                        self.set_value(field_value);
                        return;
                    }
                }
            }
        }
        *self = Self::default()
    }

    fn simplify_to_sexp_or_bits(&mut self, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) -> Option<u32> {
        match self.value {
            Some(AccessorVal::Val(Val::Symbolic(v))) => {
                if let Some(Ty::BitVec(len)) = types.get(v) {
                    let sexp = sexps.alloc(Sexp::Symbolic(*v));
                    self.set_sexp(sexp);
                    Some(*len)
                } else {
                    None
                }
            }
            Some(AccessorVal::Val(Val::Bits(bv))) => {
                self.set_bits(*bv);
                Some(bv.len())
            }
            Some(AccessorVal::Bits(bv)) => Some(bv.len()),
            _ => None,
        }
    }

    fn access_subvec(&mut self, n: u32, m: u32, types: &HashMap<Sym, Ty>, sexps: &mut SexpArena) {
        self.simplify_to_sexp_or_bits(types, sexps);

        match self.value {
            Some(AccessorVal::Sexp(sexp)) => {
                let n = sexps.alloc(Sexp::Int(n));
                let m = sexps.alloc(Sexp::Int(m));
                let extract = sexps.alloc(Sexp::List(vec![sexps.underscore, sexps.extract, n, m]));
                self.set_sexp(sexps.alloc(Sexp::List(vec![extract, sexp])))
            }
            Some(AccessorVal::Bits(bv)) => {
                if let Some(extracted) = bv.extract(n, m) {
                    self.set_bits(extracted)
                } else {
                    *self = Self::default()
                }
            }
            _ => *self = Self::default(),
        }
    }

    access_extension!(access_extz, zero_extend, B::zero_extend);
    access_extension!(access_exts, sign_extend, B::sign_extend);
}

fn generate_ite_chain<'ev, B: BV>(
    event_values: &HashMap<Name, (View<'ev, B>, &AccessorTree)>,
    ty: SexpId,
    sexps: &mut SexpArena,
) -> SexpId {
    let mut chain = sexps.alloc_default_value(ty);

    for (ev, (event_view, _)) in event_values {
        let result = event_view.value.and_then(|v| v.to_sexp(sexps));
        if let Some(id) = result {
            let ev = sexps.alloc(Sexp::Atom(*ev));
            let comparison = sexps.alloc(Sexp::List(vec![sexps.eq, ev, sexps.ev1]));
            chain = sexps.alloc(Sexp::List(vec![sexps.ite, comparison, id, chain]))
        }
    }

    chain
}

pub fn infer_accessor_type(accessors: &[Accessor], sexps: &mut SexpArena) -> SexpId {
    use Accessor::*;

    if let Some(accessor) = accessors.iter().next() {
        match accessor {
            Subvec(hi, lo) => sexps.alloc(Sexp::BitVec((hi - lo) + 1)),
            Extz(n) | Exts(n) => sexps.alloc(Sexp::BitVec(*n)),
            _ => sexps.alloc(Sexp::BitVec(64)),
        }
    } else {
        sexps.alloc(Sexp::BitVec(64))
    }
}

fn required_index_bits(n: usize) -> u32 {
    ((std::mem::size_of::<usize>() * 8) as u32) - n.saturating_sub(1).leading_zeros()
}

pub fn generate_accessor_function<'ev, B: BV, E: ModelEvent<'ev, B>, V: Borrow<E>>(
    accessor_fn: Name,
    ty: Option<SexpId>,
    accessors: &[Accessor],
    events: &[V],
    types: &HashMap<Sym, Ty>,
    shared_state: &SharedState<B>,
    symtab: &Symtab,
    sexps: &mut SexpArena,
) -> SexpId {
    use Accessor::*;

    let acctree = &AccessorTree::from_accessors(accessors);
    let mut max_events: usize = 1;

    let mut event_values: HashMap<Name, (View<'ev, B>, &AccessorTree)> = HashMap::new();

    for event in events {
        let name = event.borrow().name();
        let opcode = event.borrow().opcode();
        match event.borrow().base_events() {
            &[ev] => {
                let view = match ev {
                    Event::ReadMem { address, value, read_kind, .. } => View::new(opcode)
                        .with_special("data", value)
                        .with_special("address", address)
                        .with_value(read_kind),
                    Event::WriteMem { address, data, write_kind, .. } => View::new(opcode)
                        .with_special("data", data)
                        .with_special("address", address)
                        .with_value(write_kind),
                    Event::Abstract { name: outcome_name, primitive, args, return_value } if *primitive => {
                        // This will be the original name of the outcome in the Sail source
                        let outcome_name = zencode::decode(shared_state.symtab.to_str_demangled(*outcome_name));
                        View::new(opcode)
                            .with_name(outcome_name)
                            .with_values(&args)
                            .with_special("return", return_value)
                    }
                    Event::ReadReg(_, _, value) | Event::WriteReg(_, _, value) => View::new(opcode).with_value(value),
                    _ => View::default(),
                };
                event_values.insert(name, (view, acctree));
            }
            events => {
                max_events = usize::max(max_events, events.len());
                event_values.insert(name, (View::default(), acctree));
            }
        }
    }

    let _index_bits = required_index_bits(max_events);

    for (view, acctree) in event_values.values_mut() {
        loop {
            match acctree {
                AccessorTree::Node { elem, child } => {
                    match *elem {
                        Extz(n) => view.access_extz(*n, types, sexps),
                        Exts(n) => view.access_exts(*n, types, sexps),
                        Subvec(hi, lo) => view.access_subvec(*hi, *lo, types, sexps),
                        Tuple(n) => view.access_tuple(*n, shared_state),
                        Bits(_bitvec) => (),
                        Id(id) => view.access_literal_id(*id, sexps),
                        Field(name) => view.access_field(*name, symtab, shared_state),
                        Length(_n) => (),
                        Address => view.access_special("address"),
                        Data => view.access_special("data"),
                        Opcode => view.access_special("opcode"),
                        Return => view.access_special("return"),
                        Is(expected) => view.access_is_name(&symtab[*expected]),

                        // Should not occur as an accessortree node
                        Ctor(_) | Wildcard | Match(_) => unreachable!(),
                    }
                    *acctree = child
                }
                AccessorTree::Match { arms } => {
                    let child = view.access_match(arms, symtab, shared_state);
                    *acctree = child
                }
                AccessorTree::Leaf => break,
            }
        }
    }

    let accessor_param = sexps.alloc(Sexp::List(vec![sexps.ev1, sexps.event]));
    let accessor_params = sexps.alloc(Sexp::List(vec![accessor_param]));
    let accessor_ty = match ty {
        Some(ty) => ty,
        None => infer_accessor_type(accessors, sexps),
    };
    let accessor_ite = generate_ite_chain(&event_values, accessor_ty, sexps);

    let accessor_fn = sexps.alloc(Sexp::Atom(accessor_fn));
    sexps.alloc(Sexp::List(vec![sexps.define_fun, accessor_fn, accessor_params, accessor_ty, accessor_ite]))
}
