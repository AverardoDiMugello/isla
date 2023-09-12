// BSD 2-Clause License
//
// Copyright (c) 2023 Ben Simner
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

use isla_lib::bitvector::BV;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::sexp::{InterpretEnv, InterpretError, InterpretResult, Sexp, SexpRelation, SexpVal, LambdaFun};
use crate::sexp_lexer::{SexpLexer, Tok};
use crate::sexp_parser::SexpParser;
use isla_lib::lexer::LexError;
use lalrpop_util::ParseError;

pub mod pairwise {
    pub struct Pairs<'a, A> {
        index: (usize, usize),
        slice: &'a [A],
    }

    impl<'a, A> Pairs<'a, A> {
        pub fn from_slice(slice: &'a [A]) -> Self {
            Pairs { index: (0, 0), slice }
        }
    }

    impl<'a, A> Iterator for Pairs<'a, A> {
        type Item = (&'a A, &'a A);

        fn next(&mut self) -> Option<Self::Item> {
            self.index.1 += 1;
            if self.index.1 > self.slice.len() {
                self.index.1 = 1;
                self.index.0 += 1;
            }
            if self.index.0 >= self.slice.len() {
                return None;
            }
            Some((&self.slice[self.index.0], &self.slice[self.index.1 - 1]))
        }
    }
}

/// A value of a smtlib expression (as generated by isla)
/// is either a bitvector, a straight boolean,
/// an Event,
/// or a set of events  (represented as an Array Event Bool)
#[derive(Debug, Clone)]
enum SmtFn<'s, 'ev> {
    Lambda(LambdaFun<'s>),
    Fixed(SexpRelation<'ev>),
}

#[derive(Debug)]
pub struct Model<'s, 'ev, B> {
    env: InterpretEnv<'s, 'ev, B>,
    functions: HashMap<&'s str, SmtFn<'s, 'ev>>,
}

#[derive(Clone, Debug)]
pub enum ModelParseError<'s> {
    SmtParseError(ParseError<usize, Tok<'s>, LexError>),
    SmtInterpretError(InterpretError<'s>),
}

impl<'s> fmt::Display for ModelParseError<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SmtParseError(pe) => write!(f, "failed to parse smt: {}", pe),
            Self::SmtInterpretError(ie) => write!(f, "failed to interpret smt during parse: {}", ie),
        }
    }
}

impl<'s> Error for ModelParseError<'s> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // TODO: would like to return the inner error, but for SmtParseError it contains a non-static reference to a Tok, so cannot see how
        None
    }
}

impl<'s, 'ev, B: BV> Model<'s, 'ev, B> {
    /// Parse a model from a string of the form (model (define-fun ...) (define-fun ...) ...)
    pub fn parse(events: &[&'ev str], model: &'s str) -> Result<Self, ModelParseError<'s>> {
        let mut env = InterpretEnv::new();
        for event in events {
            env.add_event(event)
        }

        let functions = HashMap::new();
        let mut m = Model { env, functions };

        let lexer = SexpLexer::new(model);
        match SexpParser::new().parse(lexer) {
            Ok(sexp) => match sexp.dest_fn_or_list("model") {
                Some(function_sexps) => {
                    for f in function_sexps {
                        let f2 = f.clone();
                        m.record_function(f)
                            .map_err(|e| ModelParseError::SmtInterpretError(e.push_context(f2)))?;
                    }
                    Ok(m)
                }
                None => {
                    Err(ModelParseError::SmtInterpretError(InterpretError::bad_type("model must be list".to_string())))
                }
            },
            Err(e) => Err(ModelParseError::SmtParseError(e)),
        }
    }

    fn record_function(&mut self, f: Sexp<'s>) -> Result<(), InterpretError<'s>> {
        if let (Sexp::Atom(name), val) = f.dest_pair().ok_or(InterpretError::bad_function_call())? {
            if val.is_lambda() {
                self.functions.insert(name, SmtFn::Lambda(val.dest_lambda()?));
            } else if name == "IW" {
                if !val.is_atom("IW") {
                    return Err(InterpretError::unexpected_sexp("IW", &val));
                }
            } else {
                let r = val.interpret(&mut self.env)?.expect_relation(None)?;
                self.functions.insert(name, SmtFn::Fixed(r));
            }
            Ok(())
        } else {
            Err(InterpretError::bad_function_call())
        }
    }

    fn do_arg_binding(&mut self, params: &Vec<(&'s str, Sexp<'s>)>, args: &[&'ev str]) -> Result<(), InterpretError<'s>> {
        for ((param, _), ev) in params.iter().zip(args) {
            self.env.push(param, SexpVal::Event(ev));
        }
        Ok(())
    }

    fn undo_arg_binding(&mut self, params: &Vec<(&'s str, Sexp<'s>)>) -> Result<(), InterpretError<'s>> {
        for (param, _) in params.iter().rev() {
            self.env.pop(param);
        }
        Ok(())
    }

    /// given a (lambda ((x T1) (y T2) ...) SEXP)
    /// apply `args` to it and return the result
    /// (implicitly assuming boolean result)
    fn interpret_fn(&mut self, lf: &LambdaFun<'s>, args: &[&'ev str]) -> InterpretResult<'ev, 's, B> {
        // NOTE: we do not ever produce lambdas as values, instead they're only ever immediately applied to events
        // so we do not have closures and they're basically just lets
        self.do_arg_binding(&lf.params, args)?;
        let v = lf.body.interpret(&mut self.env)?;
        self.undo_arg_binding(&lf.params)?;
        Ok(v)
    }

    /// Interprets a name in the model
    pub fn interpret(&mut self, f: &str, args: &[SexpVal<'ev, B>]) -> InterpretResult<'ev, 's, B> {
        let function = self.functions.get(f).ok_or_else(|| InterpretError::unknown_function(f.to_string()))?.clone();

        match function {
            SmtFn::Fixed(r) => {
                // no args => return r
                if args.len() == 0 {
                    return Ok(SexpVal::Relation(r.clone()));
                }

                Ok(SexpVal::Bool(r.contains(args)?))
            }
            SmtFn::Lambda(lf) => {
                let args: Vec<&str> = args
                    .iter()
                    .map(|a| a.clone().into_event())
                    .collect::<Option<Vec<&str>>>()
                    .ok_or(InterpretError::bad_param_list())?;
                self.interpret_fn(&lf, args.as_slice())
            }
        }
    }

    /// Gives an entire relation as a Vec<(event,event)>
    pub fn interpret_rel(&mut self, f: &str) -> Result<Vec<(&'ev str, &'ev str)>, InterpretError<'s>> {
        let evs: Vec<&str> = self.env.events.keys().map(|s| *s).collect();
        let pairs = pairwise::Pairs::from_slice(evs.as_slice());
        let mut rel = vec![];
        for (ev1, ev2) in pairs {
            let b = self
                .interpret(f, &[SexpVal::Event(*ev1), SexpVal::Event(*ev2)])?
                .into_bool()
                .ok_or(InterpretError::not_found(f.to_string()))?;
            if b {
                rel.push((*ev1, *ev2));
            }
        }
        Ok(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use isla_lib::bitvector::b64::B64;

    #[test]
    fn test_parse() {
        let smtlib = "(model (define-fun v12331 () (_ BitVec 32) #x00000001))";
        Model::<B64>::parse(&[], smtlib).unwrap();
    }

    #[test]
    fn test_interpret_1() {
        let smtlib = "(model (define-fun dmb ((x!0 Event)) Bool false))";
        let ev = "R0";
        let mut model = Model::<B64>::parse(&[ev], smtlib).unwrap();
        let result = model.interpret("dmb", &[SexpVal::Event(ev)]).unwrap();
        assert_eq!(result, SexpVal::Bool(false));
    }

    #[test]
    fn test_interpret_2() {
        let smtlib = "(model (define-fun |0xdmb%| ((x!0 Event)) Bool false))";
        let ev = "R0";
        let mut model = Model::<B64>::parse(&[ev], smtlib).unwrap();
        let result = model.interpret("0xdmb%", &[SexpVal::Event(ev)]).unwrap();
        assert_eq!(result, SexpVal::Bool(false));
    }

    #[test]
    fn test_interpret_3() {
        let smtlib = "(model (define-fun |foo| ((x!0 Event)) Bool (let ((a!0 true)) (let ((a!0 false)) (and a!0)))))";
        let ev = "R0";
        let mut model = Model::<B64>::parse(&[ev], smtlib).unwrap();
        let result = model.interpret("foo", &[SexpVal::Event(ev)]).unwrap();
        assert_eq!(result, SexpVal::Bool(false));
    }

    #[test]
    fn test_interpret_4() {
        let smtlib = "(model (define-fun |foo| ((x!0 Event)) Bool (ite false true (not (= x!0 R0)))))";
        let ev = "R0";
        let mut model = Model::<B64>::parse(&[ev], smtlib).unwrap();
        let result = model.interpret("foo", &[SexpVal::Event(ev)]).unwrap();
        assert_eq!(result, SexpVal::Bool(false));
    }

    #[test]
    fn test_interpret_rel() {
        let smtlib = "(model (define-fun obs ((x!0 Event) (x!1 Event)) Bool
                        (or (and (= x!0 W0) (= x!1 R1))
                            (and (= x!0 IW) (= x!1 W0))
                            (and (= x!0 W1) (= x!1 R0))
                            (and (= x!0 IW) (= x!1 W1)))))";
        let evs = ["IW", "W0", "W1", "R0", "R1"];
        let mut model = Model::<B64>::parse(&evs, smtlib).unwrap();
        let result = model.interpret_rel("obs", &evs).unwrap();
        assert!(result.contains(&("W0", "R1")));
        assert!(result.contains(&("IW", "W0")));
        assert!(result.contains(&("W1", "R0")));
        assert!(result.contains(&("IW", "W1")));
        assert!(result.len() == 4);
    }
}