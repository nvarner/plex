mod nonterminal_set;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write};

use lalr::*;

use parser::nonterminal_set::NonterminalSet;
use proc_macro;
use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{self, token, Attribute, Block, Expr, Ident, Meta, NestedMeta, Pat, Type, Visibility};

/// Return the most frequent item in the given iterator, or None if it is empty.
/// Picks an arbitrary item in case of a tie.
fn most_frequent<T: Ord, I: Iterator<Item = T>>(it: I) -> Option<T> {
    let mut freq = BTreeMap::new();
    for x in it {
        *freq.entry(x).or_insert(0) += 1;
    }
    freq.into_iter().max_by_key(|&(_, f)| f).map(|(x, _)| x)
}

fn expected_one_of<S: fmt::Display>(xs: &[S]) -> String {
    let mut err_msg: String = "expected".to_string();
    for (i, x) in xs.iter().enumerate() {
        if i == 0 {
            let _ = write!(&mut err_msg, " ");
        } else if i == xs.len() - 1 {
            if i == 1 {
                let _ = write!(&mut err_msg, " or ");
            } else {
                let _ = write!(&mut err_msg, ", or ");
            }
        } else {
            let _ = write!(&mut err_msg, ", ");
        }
        let _ = write!(&mut err_msg, "{}", x);
    }
    err_msg
}

pub fn lr1_machine<'a, T, N, A, FM, FA, FR, FO>(
    grammar: &'a Grammar<T, N, A>,
    types: &BTreeMap<N, Type>,
    token_ty: Type,
    span_ty: Type,
    range_fn: Option<(Ident, Ident, Block)>,
    vis: Visibility,
    name: Ident,
    mut to_pat: FM,
    mut to_expr: FA,
    reduce_on: FR,
    priority_of: FO,
) -> Result<TokenStream, LR1Conflict<'a, T, N, A>>
where
    T: Ord + fmt::Debug + fmt::Display,
    N: Ord + fmt::Debug,
    A: fmt::Debug,
    FM: FnMut(&T) -> TokenStream,
    FA: FnMut(&N, &A, &[Symbol<T, N>]) -> (TokenStream, Vec<Option<TokenStream>>, Span),
    FR: FnMut(&Rhs<T, N, A>, Option<&T>) -> bool,
    FO: FnMut(&Rhs<T, N, A>, Option<&T>) -> i32,
{
    let actual_start = match grammar
        .rules
        .get(&grammar.start)
        .expect("Grammar didn't contain its start nonterminal")[0]
        .syms[0]
    {
        Terminal(_) => panic!("bad grammar"),
        Nonterminal(ref x) => x,
    };
    let any_ty = quote!(::std::any::Any);
    let start_ty = &types[actual_start];
    let table: LR1ParseTable<T, N, A> = grammar.lalr1(reduce_on, priority_of)?;
    let rule_fn_ids: BTreeMap<_, _> = grammar
        .rules
        .iter()
        .filter(|&(lhs, _)| *lhs != grammar.start)
        .flat_map(|(_, rhss)| {
            // Identify rules by their RHS, which should have unique addresses
            // FIXME: maybe not the best idea
            rhss.iter().map(|rhs| rhs as *const _)
        })
        .enumerate()
        .map(|(i, k)| (k, Ident::new(&format!("reduce_{}", i), Span::call_site())))
        .collect();
    let goto_fn_ids: BTreeMap<_, _> = grammar
        .rules
        .keys()
        .filter(|&lhs| *lhs != grammar.start)
        .enumerate()
        .map(|(i, lhs)| (lhs, Ident::new(&format!("goto_{}", i), Span::call_site())))
        .collect();

    let mut stmts = Vec::new();

    stmts.push(if let Some((a, b, body)) = range_fn {
        quote!(fn range(#a: #span_ty, #b: #span_ty) -> #span_ty {
            #body
        })
    } else {
        quote!(
            fn range(_a: (), _b: ()) {}
        )
    });
    stmts.push(
        quote!(fn range_array(x: &[Option<#span_ty>]) -> Option<#span_ty> {
            if let Some(lo) = x.iter().filter_map(|&x| x).next() {
                let hi = x.iter().rev().filter_map(|&x| x).next().unwrap();
                Some(range(lo, hi))
            } else {
                None
            }
        }),
    );
    // The `lhs`s which are actually unreachable; as such, their associated code should not be generated.
    let mut lhs_unreachable = BTreeSet::new();
    for (lhs, id) in &goto_fn_ids {
        let expr = if let Some(&most_freq) =
            most_frequent(table.states.iter().filter_map(|state| state.goto.get(lhs)))
        {
            let most_freq = most_freq as u32;
            let mut pats_by_dest = BTreeMap::new();
            for (ix, state) in table.states.iter().enumerate() {
                let ix = ix as u32;
                if let Some(&dest) = state.goto.get(lhs) {
                    let dest = dest as u32;
                    if dest != most_freq {
                        pats_by_dest.entry(dest).or_insert(vec![]).push(quote!(#ix));
                    }
                }
            }
            let mut arms: Vec<_> = pats_by_dest
                .into_iter()
                .map(|(dest, pats)| quote!(#(#pats)|* => #dest,))
                .collect();
            arms.push(quote!(_ => #most_freq,));
            quote!(match state { #(#arms)* })
        } else {
            // This shouldn't normally happen, but it can when `lhs` is unused in the
            // grammar.
            lhs_unreachable.insert(lhs);
            continue;
        };
        stmts.push(quote!(fn #id(state: u32) -> u32 {
            #expr
        }));
    }
    for (lhs, rhss) in &grammar.rules {
        if *lhs == grammar.start || lhs_unreachable.contains(&lhs) {
            continue;
        }
        let goto_fn = goto_fn_ids[lhs].clone();
        let lhs_ty = &types[lhs];
        for rhs in rhss.iter() {
            let (result, arg_pats, rhs_span) = to_expr(lhs, &rhs.act, &rhs.syms);
            let len = rhs.syms.len();
            let current_span_stmt = if rhs.syms.len() > 0 {
                // Make the current_span available to the user by exposing it through a macro whose name is unhygienic.
                let current_span_ident = quote_spanned!(rhs_span => current_span);
                let span_macro = quote!(
                    #[allow(unused_macros)]
                    macro_rules! span {
                        () => { #current_span_ident.unwrap() }
                    }
                );
                quote_spanned!(rhs_span =>
                    let current_span: Option<#span_ty> = {
                        let sp = range_array(&span_stack[(span_stack.len() - #len)..]);
                        let newlen = span_stack.len() - #len;
                        span_stack.truncate(newlen);
                        sp
                    };
                    #span_macro
                )
            } else {
                quote_spanned!(rhs_span =>
                    let current_span: Option<#span_ty> = None;
                )
            };
            let mut reduce_stmts = vec![current_span_stmt];

            // Deepest level
            let mut deepest_stmts = vec![];
            if rhs.syms.len() > 1 {
                let len_minus_one = rhs.syms.len() - 1;
                // XXX: Annoying syntax :(
                deepest_stmts.push(
                    quote_spanned!(rhs_span => match state_stack.len() - #len_minus_one { x => state_stack.truncate(x) };),
                );
            } else if rhs.syms.len() == 0 {
                deepest_stmts.push(quote_spanned!(rhs_span => state_stack.push(*state);));
            }
            deepest_stmts.push(quote_spanned!(
                rhs_span =>
                *state = #goto_fn(*state_stack.last().unwrap());
                let result: #lhs_ty = ( || -> #lhs_ty { #result } )();
                stack.push(Box::new(result) as Box<#any_ty>);
                span_stack.push(current_span);
            ));

            reduce_stmts.push(rhs.syms.iter().zip(arg_pats.iter().cloned()).fold(
                quote_spanned!(rhs_span => #(#deepest_stmts)*),
                |deeper, (sym, maybe_pat)| match maybe_pat {
                    // TODO: maybe use an even more precise span
                    Some(pat) => {
                        println!("{:?}", sym);
                        println!("{:?}", pat);
                        println!();
                        let ty = match *sym {
                            Terminal(_) => token_ty.clone(),
                            Nonterminal(ref n) => types[n].clone(),
                        };
                        quote_spanned!(rhs_span =>
                            if let #pat = *stack.pop().unwrap().downcast::<#ty>().unwrap() {
                                #deeper
                            }
                        )
                    }
                    None => {
                        println!("{:?}", sym);
                        quote_spanned!(rhs_span => stack.pop(); #deeper)
                    },
                },
            ));


            let fn_id = rule_fn_ids.get(&(rhs as *const _)).unwrap().clone();
            stmts.push(quote_spanned!(
                rhs_span =>
                fn #fn_id(
                    stack: &mut Vec<Box<#any_ty>>,
                    span_stack: &mut Vec<Option<#span_ty>>,
                    state_stack: &mut Vec<u32>,
                    state: &mut u32,
                ) {
                    #(#reduce_stmts)*
                }
            ));
        }
    }
    stmts.push(quote!(
        let mut stack: Vec<Box<#any_ty>> = Vec::new();
        let mut span_stack = Vec::new();
        let mut state_stack = Vec::new();
        let mut state: u32 = 0;
        let mut token_span = it.next();
    ));
    stmts.push({
        let state_arms = table.states.iter().enumerate().map(|(ix, state)| {
            let mut arms = vec![];
            let mut reduce_arms = Vec::new();
            let mut expected = vec![];
            for (&tok, action) in &state.lookahead {
                expected.push(format!("`{}`", tok));
                let tok_pat = to_pat(tok);
                let pat = quote!(Some((#tok_pat, _)));
                let arm_expr = match *action {
                    LRAction::Shift(dest) => dest as u32,
                    LRAction::Reduce(_, rhs) => {
                        reduce_arms.push((rhs as *const _, pat));
                        continue;
                    }
                    LRAction::Accept => unreachable!(),
                };
                arms.push(quote!(#pat => #arm_expr,));
            }
            if let Some(ref action) = state.eof {
                expected.push("end of file".into());
                let pat = quote!(None);
                match *action {
                    LRAction::Shift(_) => unreachable!(),
                    LRAction::Reduce(_, rhs) => {
                        reduce_arms.push((rhs as *const _, pat));
                    }
                    LRAction::Accept => {
                        arms.push(
                            quote!(#pat => return Ok(*stack.pop().unwrap().downcast::<#start_ty>().unwrap()),),
                        );
                    }
                };
            }
            for (rhs_ptr, pat) in reduce_arms.into_iter() {
                let reduce_fn = rule_fn_ids[&rhs_ptr].clone();
                arms.push(quote!(#pat => {
                    #reduce_fn(&mut stack, &mut span_stack, &mut state_stack, &mut state);
                    continue
                }));
            }
            let err_msg = expected_one_of(&expected);
            arms.push(quote!(_ => return Err((token_span, #err_msg)),));
            let ix = ix as u32;
            quote!(#ix => match &token_span { #(#arms)* })
        });
        quote!(
            loop {
                let next_state = match state {
                    #(#state_arms)*
                    _ => unreachable!(),
                };
                match token_span {
                    Some((token, span)) => {
                        stack.push(Box::new(token) as Box<#any_ty>);
                        span_stack.push(Some(span));
                    }
                    None => unreachable!(),
                };
                state_stack.push(state);
                token_span = it.next();
                state = next_state;
            }
        )
    });
    // `quote` bug: can't quote `'static`, so use `&'a str` for any `'a`. hopefully this is fine.
    Ok(quote!(
        #vis fn #name<'a, I: Iterator<Item=(#token_ty, #span_ty)>>(mut it: I) -> Result<#start_ty, (Option<(#token_ty, #span_ty)>, &'a str)> {
            #(#stmts)*
        }
    ))
}

#[derive(Debug)]
enum RuleRhsItem {
    Match(NonterminalSet),
    /// (match_to, bind_to)
    MatchBind(NonterminalSet, Pat),
}

impl RuleRhsItem {
    fn match_to(&self) -> &NonterminalSet {
        match *self {
            RuleRhsItem::Match(ref match_to) | RuleRhsItem::MatchBind(ref match_to, _) => match_to,
        }
    }
}

#[derive(Debug)]
struct Rule {
    rhs: Vec<RuleRhsItem>,
    // `None` if `rhs` is empty
    rhs_span: proc_macro::Span,
    action: TokenStream,
    exclusions: Vec<NonterminalSet>,
    exclude_eof: bool,
    priority: i32,
}

fn parse_rules(input: ParseStream) -> syn::Result<Vec<Rule>> {
    let mut rules = vec![];
    while !input.is_empty() {
        // FIXME: Make some nicer error messages.
        let mut exclusions = Vec::new();
        let mut exclude_eof = false;
        let mut priority = 0;
        let attrs = Attribute::parse_outer(input)?;
        for attr in attrs {
            match attr.parse_meta()? {
                Meta::List(ref list) if list.path.get_ident().unwrap() == "no_reduce" => {
                    for token in &list.nested {
                        if let NestedMeta::Meta(Meta::NameValue(ref nval)) = *token {
                            let ident = nval.path.get_ident().unwrap();

                            if ident == "EOF" {
                                exclude_eof = true;
                            } else {
                                exclusions.push(NonterminalSet::new_singleton(ident.clone()));
                            }
                        } else {
                            // FIXME bad span here
                            list.paren_token
                                .span
                                .unstable()
                                .error("invalid syntax: no_reduce list includes a non-token")
                                .emit();
                        }
                    }
                }
                Meta::NameValue(ref nval) if nval.path.get_ident().unwrap() == "overriding" => {
                    priority = 1;
                }
                meta => {
                    // FIXME non-ideal span
                    meta.path()
                        .span()
                        .unstable()
                        .error("unknown attribute")
                        .emit();
                }
            }
        }
        let mut rhs = vec![];
        let mut sp_lo = None;
        let mut sp_hi = None;
        while !input.peek(Token![=>]) {
            let match_to: Pat = input.parse()?;
            if sp_lo.is_none() {
                sp_lo = Some(match_to.span());
            }
            rhs.push(if input.peek(token::Bracket) {
                let inner;
                let bracket = bracketed!(inner in input);
                sp_hi = Some(bracket.span);
                let bind_to = inner.parse()?;
                if !inner.is_empty() {
                    return Err(inner.error("unexpected token after pattern"));
                }
                RuleRhsItem::MatchBind(NonterminalSet(match_to), bind_to)
            } else {
                sp_hi = Some(match_to.span());
                RuleRhsItem::Match(NonterminalSet(match_to))
            });
        }
        let arrow = input.parse::<Token![=>]>()?;
        let arrow_span = arrow.spans[0]
            .unstable()
            .join(arrow.spans[1].unstable())
            .expect("fat arrow has invalid spans");
        let rhs_span = sp_lo
            .map_or(arrow_span, Span::unstable)
            .join(sp_hi.map_or(arrow_span, Span::unstable))
            .expect("FIXME: you did something bad");
        // Like in a `match` expression, braced block doesn't require a comma before the next rule.
        let optional_comma = input.peek(token::Brace);
        let action: Expr = input.parse()?;
        rules.push(Rule {
            rhs,
            rhs_span,
            action: action.into_token_stream(),
            exclusions,
            exclude_eof,
            priority,
        });
        match <Token![,]>::parse(input) {
            Ok(_) => {}
            Err(e) => {
                if !input.is_empty() && !optional_comma {
                    return Err(e);
                }
            }
        }
    }
    Ok(rules)
}

struct RuleSet {
    lhs: Ident,
    return_ty: Type,
    rules: Vec<Rule>,
}

impl Parse for RuleSet {
    fn parse(input: ParseStream) -> syn::Result<RuleSet> {
        Ok(RuleSet {
            lhs: input.parse()?,
            return_ty: {
                input.parse::<Token![:]>()?;
                input.parse()?
            },
            rules: {
                let rule_content;
                braced!(rule_content in input);
                parse_rules(&rule_content)?
            },
        })
    }
}

struct Parser {
    vis: Visibility,
    name: Ident,
    token_ty: Type,
    span_ty: Type,
    range_fn: Option<(Ident, Ident, Block)>,
    rule_sets: Vec<RuleSet>,
}

impl Parse for Parser {
    fn parse(input: ParseStream) -> syn::Result<Parser> {
        let span_ty;
        Ok(Parser {
            vis: input.parse()?,
            name: {
                input.parse::<Token![fn]>()?;
                input.parse()?
            },
            token_ty: {
                let inner;
                parenthesized!(inner in input);
                let token = inner.parse()?;
                inner.parse::<Token![,]>()?;
                span_ty = inner.parse()?;
                if !inner.is_empty() {
                    return Err(inner.error("unexpected token after span type"));
                }
                input.parse::<Token![;]>()?;
                token
            },
            span_ty,
            range_fn: {
                if input.peek(token::Paren) {
                    let inner;
                    parenthesized!(inner in input);
                    let a = inner.parse()?;
                    inner.parse::<Token![,]>()?;
                    let b = inner.parse()?;
                    if !inner.is_empty() {
                        return Err(inner.error("unexpected token after second range"));
                    }
                    let body = input.parse::<Block>()?;
                    Some((a, b, body))
                } else {
                    None
                }
            },
            rule_sets: {
                let mut r = vec![];
                while !input.is_empty() {
                    r.push(input.parse()?);
                }
                r
            },
        })
    }
}

fn pretty_rule(lhs: Ident, syms: &[Symbol<NonterminalSet, NonterminalSet>]) -> String {
    let mut r = String::new();
    let _ = write!(&mut r, "{} ->", lhs);
    for sym in syms.iter() {
        let _ = write!(&mut r, " {}", sym);
    }
    r
}

// Pretty-print an item set, for error messages.
fn pretty(x: &ItemSet<NonterminalSet, NonterminalSet, &Rule>, pad: &str) -> String {
    let mut r = String::new();
    let mut first = true;
    for item in x.items.iter() {
        if first {
            first = false;
        } else {
            let _ = write!(&mut r, "\n{}", pad);
        }
        let _ = write!(&mut r, "{} ->", item.lhs);
        for j in 0..item.pos {
            let _ = write!(&mut r, " {}", item.rhs.syms[j]);
        }
        let _ = write!(&mut r, " •");
        for j in item.pos..item.rhs.syms.len() {
            let _ = write!(&mut r, " {}", item.rhs.syms[j]);
        }
    }
    r
}

pub fn parser(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let parser = parse_macro_input!(input as Parser);
    let fake_rule; // N.B. must go before `rules` to appease dropck
    let mut rules = BTreeMap::new();
    let mut types = BTreeMap::new();
    let mut start = None;
    for rule_set in &parser.rule_sets {
        // parse "LHS: Type {"
        let lhs = &rule_set.lhs;
        if start.is_none() {
            start = Some(NonterminalSet::new_singleton(lhs.clone()));
        }
        match rules.entry(NonterminalSet::new_singleton(lhs.clone())) {
            Entry::Occupied(ent) => {
                lhs.span()
                    .unstable()
                    .error("duplicate nonterminal")
                    .span_note(ent.key().0.span().unstable(), "first definition here")
                    .emit();
            }
            Entry::Vacant(ent) => {
                types.insert(
                    NonterminalSet::new_singleton(lhs.clone()),
                    rule_set.return_ty.clone(),
                );
                ent.insert(&rule_set.rules);
            }
        }
    }
    let start = if let Some(start) = start {
        start
    } else {
        Span::call_site()
            .unstable()
            .error("at least one nonterminal is required")
            .emit();
        return proc_macro::TokenStream::new();
    };
    let mut rules: BTreeMap<NonterminalSet, Vec<_>> = rules
        .into_iter()
        .map(|(lhs, rules)| {
            let rhss = rules
                .iter()
                .map(|rule| {
                    // figure out which symbols in `rule` are nonterminals vs terminals
                    let syms = rule
                        .rhs
                        .iter()
                        .map(|tok| {
                            let match_to = tok.match_to().clone();
                            if types.contains_key(&match_to) {
                                Nonterminal(match_to)
                            } else {
                                Terminal(match_to)
                            }
                        })
                        .collect();
                    Rhs {
                        syms: syms,
                        act: rule,
                    }
                })
                .collect();
            (lhs, rhss)
        })
        .collect();
    let fake_start = NonterminalSet::new_singleton(Ident::new("__FIXME__start", Span::call_site()));
    fake_rule = Rule {
        rhs: vec![],
        rhs_span: proc_macro::Span::call_site(),
        action: quote!(),
        exclusions: vec![],
        exclude_eof: false,
        priority: -1,
    };
    rules.insert(
        fake_start.clone(),
        vec![Rhs {
            syms: vec![Nonterminal(start)],
            act: &fake_rule,
        }],
    );
    let grammar = Grammar {
        rules,
        start: fake_start,
    };
    lr1_machine::<NonterminalSet, NonterminalSet, _, _, _, _, _>(
        &grammar,
        &types,
        parser.token_ty,
        parser.span_ty,
        parser.range_fn,
        parser.vis,
        parser.name,
        |nonterminal_set| {
            let pat = &nonterminal_set.0;
            quote!(#pat)
        },
        |lhs, act, syms| {
            let mut expr = act.action.clone().into_token_stream();
            let mut args = vec![];
            debug_assert_eq!(syms.len(), act.rhs.len());
            for (i, (sym, x)) in syms.iter().zip(&act.rhs).enumerate() {
                args.push(match *x {
                    RuleRhsItem::MatchBind(ref match_to, ref bind_to) => Some(bind_to.clone().into_token_stream()),
                    RuleRhsItem::Match(NonterminalSet(ref match_to)) => Some(match_to.clone().into_token_stream()),
                });
            }

            // XXX: should be a cargo feature (?)
            if false {
                let rule_str = pretty_rule(lhs.get_from_singleton(), syms);
                expr = quote!({
                    println!("reduce by {}", #rule_str);
                    #expr
                });
            }

            (expr, args, act.rhs_span.into())
        },
        |rhs, token| match token {
            Some(id) => !rhs.act.exclusions.contains(&id),
            None => !rhs.act.exclude_eof,
        },
        |rhs, _| rhs.act.priority,
    )
    .unwrap_or_else(|conflict| {
        match conflict {
            LR1Conflict::ReduceReduce {
                state,
                token,
                r1,
                r2,
            } => {
                // FIXME: wtf this span
                proc_macro::Span::call_site()
                    .error(format!(
                        "reduce-reduce conflict:
state: {}
token: {}",
                        pretty(&state, "       "),
                        match token {
                            Some(id) => id.to_string(),
                            None => "EOF".to_string(),
                        }
                    ))
                    .span_note(r1.1.act.rhs_span, "conflicting rule")
                    .span_note(r2.1.act.rhs_span, "conflicting rule")
                    .emit();
            }
            LR1Conflict::ShiftReduce { state, token, rule } => {
                rule.1
                    .act
                    .rhs_span
                    .error(format!(
                        "shift-reduce conflict:
state: {}
token: {}",
                        pretty(&state, "       "),
                        match token {
                            Some(id) => id.to_string(),
                            None => "EOF".to_string(),
                        }
                    ))
                    .emit();
            }
        };
        TokenStream::new()
    })
    .into()
}
