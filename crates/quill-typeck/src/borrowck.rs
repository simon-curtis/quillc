//! AST-level loan checking for the body forms retained by `quill-syntax`.
//!
//! This is deliberately kept separate from type inference. It tracks a
//! reference's borrowed place until the last later use of its local binding,
//! then checks overlapping reads, writes, and new borrows.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use super::{
    BinaryOperator, SignatureIndex, StructSignature, Ty, infer_with_structs, is_copy_type, matching_signatures,
    method_signatures, type_from_syntax,
};
use quill_syntax::{
    BodyExpr, BodyExprKind, BodyStatement, ClosureParameter, FunctionItem, MatchPattern,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Diagnostic {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Place {
    root: String,
    projection: Vec<String>,
}

impl Place {
    fn overlaps(&self, other: &Self) -> bool {
        if self.root != other.root {
            return false;
        }
        self.projection
            .iter()
            .zip(&other.projection)
            .all(|(left, right)| left == right || left == "[*]" || right == "[*]")
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Loan {
    holder: Option<String>,
    holder_projection: Vec<String>,
    parent: Option<String>,
    place: Place,
    lifetimes: Vec<String>,
    mutable: bool,
    two_phase: bool,
    origin: Range<usize>,
}

#[derive(Clone, Debug, Default)]
struct State {
    loans: Vec<Loan>,
}

impl State {
    fn for_holder(&self, name: &str) -> impl Iterator<Item = &Loan> {
        self.loans
            .iter()
            .filter(move |loan| {
                loan.holder.as_deref() == Some(name) && loan.holder_projection.is_empty()
            })
    }

    fn all_for_holder(&self, name: &str) -> impl Iterator<Item = &Loan> {
        self.loans
            .iter()
            .filter(move |loan| loan.holder.as_deref() == Some(name))
    }

    fn held_at(&self, root: &str, projection: &[String]) -> Vec<Loan> {
        self.loans
            .iter()
            .filter(|loan| {
                loan.holder.as_deref() == Some(root)
                    && loan.holder_projection.starts_with(projection)
            })
            .cloned()
            .map(|mut loan| {
                loan.holder_projection.drain(..projection.len());
                loan
            })
            .collect()
    }

    fn replace_held_at(&mut self, root: &str, projection: &[String], loans: Vec<Loan>) {
        self.loans.retain(|loan| {
            loan.holder.as_deref() != Some(root)
                || !loan
                    .holder_projection
                    .iter()
                    .zip(projection)
                    .all(|(held, assigned)| {
                        held == assigned || held == "[*]" || assigned == "[*]"
                    })
        });
        self.loans.extend(loans.into_iter().map(|mut loan| {
            loan.holder = Some(root.to_owned());
            loan.holder_projection.splice(0..0, projection.iter().cloned());
            loan
        }));
    }

    fn replace_holder(&mut self, name: &str, loans: Vec<Loan>) {
        self.loans
            .retain(|loan| loan.holder.as_deref() != Some(name));
        self.loans.extend(loans.into_iter().map(|mut loan| {
            loan.holder = Some(name.to_owned());
            loan
        }));
    }

    fn merge(&mut self, other: State) {
        for loan in other.loans {
            if !self.loans.contains(&loan) {
                self.loans.push(loan);
            }
        }
    }

    fn same_loans(&self, other: &State) -> bool {
        self.loans.len() == other.loans.len()
            && self.loans.iter().all(|loan| other.loans.contains(loan))
    }

    fn accessed_through(&self, loan: &Loan, through: Option<&str>) -> bool {
        let Some(through) = through else {
            return false;
        };
        let mut pending = vec![through.to_owned()];
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            if loan.holder.as_deref() == Some(&current) {
                return true;
            }
            pending.extend(
                self
                .for_holder(&current)
                .filter_map(|child| child.parent.clone())
            );
        }
        false
    }

    fn descends_from(&self, loan: &Loan, holder: &str) -> bool {
        let mut pending = loan.parent.iter().cloned().collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some(parent) = pending.pop() {
            if !visited.insert(parent.clone()) {
                continue;
            }
            if parent == holder {
                return true;
            }
            pending.extend(
                self.for_holder(&parent)
                    .filter_map(|ancestor| ancestor.parent.clone()),
            );
        }
        false
    }
}

pub(super) fn check(
    function: &FunctionItem,
    body: &BodyExpr,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    local_types: &HashMap<String, Ty>,
) -> Vec<Diagnostic> {
    let mut local_roots = HashSet::new();
    local_roots.extend(
        function
            .parameters
            .iter()
            .filter_map(|parameter| parameter.name.clone()),
    );
    collect_bindings(body, &mut local_roots);
    let mut checker = Checker {
        diagnostics: Vec::new(),
        local_roots,
        mutable_locals: function
            .parameters
            .iter()
            .filter_map(|parameter| {
                parameter.name.as_ref().map(|name| {
                    (
                        name.clone(),
                        parameter.mutable,
                    )
                })
            })
            .collect(),
        signatures,
        struct_signatures,
        local_types: local_types.clone(),
        expected_return_lifetimes: return_lifetime_contract(function),
    };
    let mut state = State::default();
    for parameter in &function.parameters {
        let Some(name) = &parameter.name else {
            continue;
        };
        let Some(reference) = parameter.type_text.trim().strip_prefix('&') else {
            continue;
        };
        let reference = reference.trim_start();
        let referent = if reference.starts_with('\'') {
            reference
                .find(char::is_whitespace)
                .map_or("", |index| reference[index..].trim_start())
        } else {
            reference
        };
        let mutable = referent.starts_with("mut ");
        state.loans.push(Loan {
            holder: Some(name.clone()),
            holder_projection: Vec::new(),
            parent: None,
            place: Place {
                root: format!("\0input:{name}"),
                projection: Vec::new(),
            },
            lifetimes: parameter_lifetimes(&parameter.type_text, name),
            mutable,
            two_phase: false,
            origin: parameter.span.clone(),
        });
    }
    let returned_loans = checker.expression(body, &mut state);
    checker.validate_return_lifetimes(&returned_loans);
    checker.diagnostics
}

pub(super) fn captures(
    body: &BodyExpr,
    parameters: &[ClosureParameter],
) -> HashSet<String> {
    let mut names = HashSet::new();
    expression_names(body, &mut names);
    let mut declared = HashSet::new();
    collect_bindings(body, &mut declared);
    for parameter in parameters {
        match parameter {
            ClosureParameter::Identifier(name) => {
                declared.insert(name.clone());
            }
            ClosureParameter::Tuple(items) => declared.extend(items.iter().cloned()),
        }
    }
    names.retain(|name| !declared.contains(name));
    names
}

struct Checker<'a, 'b> {
    diagnostics: Vec<Diagnostic>,
    local_roots: HashSet<String>,
    mutable_locals: HashMap<String, bool>,
    signatures: &'a SignatureIndex<'b>,
    struct_signatures: &'a [StructSignature],
    local_types: HashMap<String, Ty>,
    expected_return_lifetimes: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiverMode {
    Value,
    Shared,
    Mutable,
}

impl Checker<'_, '_> {
    fn report(&mut self, span: Range<usize>, message: impl Into<String>) {
        let diagnostic = Diagnostic {
            span,
            message: message.into(),
        };
        if !self.diagnostics.contains(&diagnostic) {
            self.diagnostics.push(diagnostic);
        }
    }

    fn validate_return_lifetimes(&mut self, loans: &[Loan]) {
        for loan in loans {
            if self.local_roots.contains(&loan.place.root) {
                self.report(
                    loan.origin.clone(),
                    format!(
                        "error[E0515]: cannot return reference to local variable `{}`",
                        loan.place.root
                    ),
                );
            } else if let Some(expected) = &self.expected_return_lifetimes
                && !loan.lifetimes.iter().any(|lifetime| expected.contains(lifetime))
            {
                self.report(
                    loan.origin.clone(),
                    "error[E0621]: explicit lifetime required to return this reference",
                );
            }
        }
    }

    fn infer_local_type(&mut self, expression: &BodyExpr) -> Ty {
        let mut ignored = Vec::new();
        infer_with_structs(
            expression,
            &mut self.local_types,
            self.signatures,
            self.struct_signatures,
            &Ty::Unknown,
            &mut ignored,
        )
    }

    fn check_move_access(&mut self, expression: &BodyExpr, state: &State) {
        let ty = self.infer_local_type(expression);
        if ty == Ty::Unknown
            || matches!(ty, Ty::Reference(_, _))
            || is_copy_type(&ty, self.struct_signatures)
        {
            return;
        }
        let Some((place, through)) = self.place(expression, state) else {
            return;
        };
        if through.is_some() {
            // The move checker diagnoses attempts to move a referent through
            // a borrowed pointer; this check is for moving the owner itself.
            return;
        }
        if state.loans.iter().any(|loan| loan.place.overlaps(&place)) {
            self.report(
                expression.span.clone(),
                format!(
                    "error[E0505]: cannot move out of `{}` because it is borrowed",
                    place.root
                ),
            );
        }
    }

    fn access(
        &mut self,
        place: &Place,
        write: bool,
        through: Option<&str>,
        state: &State,
        span: Range<usize>,
    ) {
        if write {
            if let Some(holder) = through {
                if state.for_holder(holder).any(|loan| !loan.mutable) {
                    self.report(
                        span,
                        format!(
                            "error[E0594]: cannot assign to data in an immutable reference `{holder}`"
                        ),
                    );
                    return;
                }
            }
        }
        for loan in &state.loans {
            if loan.place.overlaps(place)
                && through.is_some_and(|holder| {
                    state.for_holder(holder).any(|active| active.mutable)
                        && state.descends_from(loan, holder)
                })
            {
                self.report(
                    span.clone(),
                    format!(
                        "error[E0503]: cannot use `{}` while a reborrow of its mutable reference is active",
                        place.root
                    ),
                );
                break;
            }
            if !loan.place.overlaps(place)
                || state.accessed_through(loan, through)
                || (loan.two_phase && !write)
            {
                continue;
            }
            if write || loan.mutable {
                let message = if write {
                    format!(
                        "error[E0506]: cannot assign to `{}` because it is borrowed",
                        place.root
                    )
                } else {
                    format!(
                        "error[E0503]: cannot use `{}` because it was mutably borrowed",
                        place.root
                    )
                };
                self.report(span.clone(), message);
                break;
            }
        }
    }

    fn mutable_access_allowed(
        &self,
        place: &Place,
        through: Option<&str>,
        state: &State,
    ) -> bool {
        if let Some(holder) = through {
            return state.for_holder(holder).any(|loan| loan.mutable);
        }
        self.mutable_locals
            .get(&place.root)
            .copied()
            .unwrap_or(true)
    }

    fn method_receiver_mode(
        &self,
        receiver: &BodyExpr,
        method: &str,
    ) -> Option<(ReceiverMode, Vec<usize>, Vec<String>)> {
        let mut local_types = self.local_types.clone();
        let mut ignored = Vec::new();
        let receiver_type = infer_with_structs(
            receiver,
            &mut local_types,
            self.signatures,
            self.struct_signatures,
            &Ty::Unknown,
            &mut ignored,
        );
        let candidates = method_signatures(&self.signatures.methods, &receiver_type, method);
        if let Some(function) = candidates.first() {
            let receiver_parameter = function.parameters.first()?;
            let receiver_type = receiver_parameter.type_text.trim_start();
            let mode = if is_mutable_reference_type(receiver_type) {
                ReceiverMode::Mutable
            } else if receiver_type.starts_with('&') {
                ReceiverMode::Shared
            } else {
                ReceiverMode::Value
            };
            return Some((
                mode,
                returned_borrowed_parameters(function),
                function
                    .parameters
                    .iter()
                    .skip(1)
                    .map(|parameter| parameter.type_text.clone())
                    .collect(),
            ));
        }
        let mode = if matches!(
            method,
            "append"
                | "as_mut_slice"
                | "clear"
                | "extend"
                | "first_mut"
                | "get_mut"
                | "insert"
                | "iter_mut"
                | "last_mut"
                | "pop"
                | "push"
                | "push_str"
                | "remove"
                | "reserve"
                | "retain"
                | "reverse"
                | "sort"
                | "truncate"
                | "values_mut"
        ) {
            ReceiverMode::Mutable
        } else if matches!(
            method,
            "as_str"
                | "as_bytes"
                | "as_ref"
                | "as_slice"
                | "iter"
                | "keys"
                | "values"
                | "get"
                | "first"
                | "last"
        ) {
            ReceiverMode::Shared
        } else {
            return None;
        };
        let returns_receiver = matches!(
            method,
            "as_str"
                | "as_bytes"
                | "as_ref"
                | "as_slice"
                | "get"
                | "get_mut"
                | "iter"
                | "iter_mut"
                | "keys"
                | "values"
                | "values_mut"
                | "first"
                | "first_mut"
                | "last"
                | "last_mut"
        );
        Some((
            mode,
            if returns_receiver { vec![0] } else { Vec::new() },
            Vec::new(),
        ))
    }

    fn create_loan(
        &mut self,
        place: Place,
        mutable: bool,
        through: Option<&str>,
        span: Range<usize>,
        state: &State,
    ) -> Loan {
        for existing in &state.loans {
            let is_parent = state.accessed_through(existing, through);
            if (is_parent && (!mutable || existing.mutable))
                || (existing.two_phase && !mutable)
                || !existing.place.overlaps(&place)
                || !(mutable || existing.mutable)
            {
                continue;
            }
            let message = if mutable && existing.mutable {
                format!(
                    "error[E0499]: cannot borrow `{}` as mutable more than once at a time",
                    place.root
                )
            } else if mutable {
                format!(
                    "error[E0502]: cannot borrow `{}` as mutable because it is also borrowed as immutable",
                    place.root
                )
            } else {
                format!(
                    "error[E0502]: cannot borrow `{}` as immutable because it is also borrowed as mutable",
                    place.root
                )
            };
            self.report(span.clone(), message);
            break;
        }
        Loan {
            holder: None,
            holder_projection: Vec::new(),
            parent: through.map(str::to_owned),
            lifetimes: through.map_or_else(Vec::new, |holder| {
                state
                    .for_holder(holder)
                    .flat_map(|loan| loan.lifetimes.iter().cloned())
                    .collect()
            }),
            place,
            mutable,
            two_phase: false,
            origin: span,
        }
    }

    fn expression(&mut self, expression: &BodyExpr, state: &mut State) -> Vec<Loan> {
        match &expression.kind {
            BodyExprKind::Path(name) => {
                let aliases = state.for_holder(name).cloned().collect::<Vec<_>>();
                let is_reference = matches!(self.local_types.get(name), Some(Ty::Reference(_, _)));
                if is_reference && !aliases.is_empty() {
                    for loan in &aliases {
                        self.access(
                            &loan.place,
                            false,
                            Some(name),
                            state,
                            expression.span.clone(),
                        );
                    }
                    aliases
                } else {
                    self.access(
                        &Place {
                            root: name.clone(),
                            projection: Vec::new(),
                        },
                        false,
                        None,
                        state,
                        expression.span.clone(),
                    );
                    state.held_at(name, &[])
                }
            }
            BodyExprKind::Reference {
                mutable,
                expression: borrowed,
            } => {
                let Some((place, through)) = self.place_for_borrow(borrowed, state) else {
                    return Vec::new();
                };
                if *mutable && !self.mutable_access_allowed(&place, through.as_deref(), state) {
                    self.report(
                        expression.span.clone(),
                        format!(
                            "error[E0596]: cannot borrow `{}` as mutable because it is not declared mutable",
                            place.root
                        ),
                    );
                    return Vec::new();
                }
                vec![self.create_loan(
                    place,
                    *mutable,
                    through.as_deref(),
                    expression.span.clone(),
                    state,
                )]
            }
            BodyExprKind::Parenthesized(inner) => self.expression(inner, state),
            BodyExprKind::Dereference(inner) => {
                let aliases = self.expression_place_alias(inner, state);
                if let Some(name) = simple_path(inner) {
                    self.access(
                        &Place {
                            root: name,
                            projection: Vec::new(),
                        },
                        false,
                        None,
                        state,
                        expression.span.clone(),
                    );
                }
                if aliases.is_empty() {
                    self.expression(inner, state);
                }
                for loan in &aliases {
                    self.access(
                        &loan.place,
                        false,
                        loan.holder.as_deref(),
                        state,
                        expression.span.clone(),
                    );
                }
                aliases
            }
            BodyExprKind::Block { statements, tail } => {
                self.block(statements, tail.as_deref(), state)
            }
            BodyExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expression(condition, state);
                let incoming = state.clone();
                let mut then_state = incoming.clone();
                let mut returned_loans = self.expression(then_branch, &mut then_state);
                let mut else_state = incoming;
                if let Some(otherwise) = else_branch {
                    for loan in self.expression(otherwise, &mut else_state) {
                        if !returned_loans.contains(&loan) {
                            returned_loans.push(loan);
                        }
                    }
                }
                then_state.merge(else_state);
                *state = then_state;
                returned_loans
            }
            BodyExprKind::Match { scrutinee, arms } => {
                self.check_move_access(scrutinee, state);
                let scrutinee_loans = self.expression(scrutinee, state);
                let incoming = state.clone();
                let mut joined = State::default();
                let mut returned_loans = Vec::new();
                for arm in arms {
                    let mut arm_state = incoming.clone();
                    let bindings = pattern_bindings(&arm.pattern);
                    for name in &bindings {
                        arm_state.replace_holder(name, scrutinee_loans.clone());
                    }
                    for loan in self.expression(&arm.body, &mut arm_state) {
                        if !returned_loans.contains(&loan) {
                            returned_loans.push(loan);
                        }
                    }
                    for name in bindings {
                        arm_state
                            .loans
                            .retain(|loan| loan.holder.as_deref() != Some(&name));
                        arm_state
                            .loans
                            .extend(incoming.all_for_holder(&name).cloned());
                    }
                    joined.merge(arm_state);
                }
                *state = joined;
                returned_loans
            }
            BodyExprKind::Call {
                callee, arguments, ..
            } => {
                let baseline = state.loans.len();
                let signature = matching_signatures(self.signatures, callee)
                    .into_iter()
                    .find(|function| function.parameters.len() == arguments.len());
                let mut argument_loans = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    if let Some(parameter) = signature
                        .and_then(|function| function.parameters.get(index))
                        && !parameter.type_text.trim_start().starts_with('&')
                    {
                        self.check_move_access(argument, state);
                    }
                    let mut loans = self.expression(argument, state);
                    if let Some(parameter) = signature.and_then(|function| function.parameters.get(index))
                        && parameter.type_text.trim_start().starts_with('&')
                    {
                        let mutable = is_mutable_reference_type(&parameter.type_text);
                        for loan in &mut loans {
                            if let Some(holder) = loan.holder.clone() {
                                *loan = self.create_loan(
                                    loan.place.clone(),
                                    mutable,
                                    Some(&holder),
                                    argument.span.clone(),
                                    state,
                                );
                            } else if !mutable {
                                loan.mutable = false;
                            }
                        }
                    }
                    for mut temporary in loans.iter().cloned() {
                        temporary.holder = None;
                        state.loans.push(temporary);
                    }
                    argument_loans.push(loans);
                }
                let returned = signature
                    .map(|function| {
                        returned_borrowed_parameters(function)
                            .into_iter()
                            .filter_map(|index| argument_loans.get(index))
                            .flatten()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                state.loans.truncate(baseline);
                returned
            }
            BodyExprKind::Tuple(arguments)
            | BodyExprKind::ArrayLiteral(arguments)
            | BodyExprKind::FixedArrayLiteral(arguments) => {
                for argument in arguments {
                    self.check_move_access(argument, state);
                }
                self.evaluate_projected(
                    arguments
                        .iter()
                        .enumerate()
                        .map(|(index, expression)| (index.to_string(), expression)),
                    state,
                )
            }
            BodyExprKind::ArrayRepeat { element, .. } => {
                self.check_move_access(element, state);
                self.expression(element, state)
            }
            BodyExprKind::MacroInvocation(macro_expression) => {
                self.evaluate_many(macro_expression.arguments.iter(), state);
                Vec::new()
            }
            BodyExprKind::MethodCall {
                receiver,
                arguments,
                method,
            } => {
                let mode = self
                    .method_receiver_mode(receiver, method)
                    .unwrap_or((ReceiverMode::Value, Vec::new(), Vec::new()));
                // A by-value receiver consumes its value; only reference
                // receivers participate in alias checks.
                if mode.0 == ReceiverMode::Value {
                    self.check_move_access(receiver, state);
                    self.expression(receiver, state);
                    self.evaluate_many(arguments.iter(), state);
                    return Vec::new();
                }
                self.expression(receiver, state);
                let Some((place, through)) = self.place(receiver, state) else {
                    self.expression(receiver, state);
                    self.evaluate_many(arguments.iter(), state);
                    return Vec::new();
                };
                if mode.0 == ReceiverMode::Mutable
                    && !self.mutable_access_allowed(&place, through.as_deref(), state)
                {
                    self.report(
                        expression.span.clone(),
                        format!(
                            "error[E0596]: cannot borrow `{}` as mutable because it is not declared mutable",
                            place.root
                        ),
                    );
                    self.evaluate_many(arguments.iter(), state);
                    return Vec::new();
                }

                let baseline = state.loans.len();
                let receiver_loan = if mode.0 == ReceiverMode::Mutable {
                    Loan {
                        holder: None,
                        holder_projection: Vec::new(),
                        parent: through.clone(),
                        lifetimes: through.as_deref().map_or_else(Vec::new, |holder| {
                            state
                                .for_holder(holder)
                                .flat_map(|loan| loan.lifetimes.iter().cloned())
                                .collect()
                        }),
                        place: place.clone(),
                        mutable: true,
                        two_phase: true,
                        origin: expression.span.clone(),
                    }
                } else {
                    self.create_loan(
                        place.clone(),
                        false,
                        through.as_deref(),
                        expression.span.clone(),
                        state,
                    )
                };
                state.loans.push(receiver_loan.clone());

                // Keep argument borrows alive together with the receiver
                // reservation. This permits ordinary shared reads during a
                // two-phase `&mut self` borrow, then checks them at activation.
                let mut argument_loans = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    let mut loans = self.expression(argument, state);
                    let parameter_type = mode.2.get(index);
                    if parameter_type.is_some_and(|type_text| !type_text.trim_start().starts_with('&')) {
                        self.check_move_access(argument, state);
                    }
                    if let Some(parameter_type) = parameter_type
                        && parameter_type.trim_start().starts_with('&')
                    {
                        let mutable = is_mutable_reference_type(parameter_type);
                        for loan in &mut loans {
                            if let Some(holder) = loan.holder.clone() {
                                *loan = self.create_loan(
                                    loan.place.clone(),
                                    mutable,
                                    Some(&holder),
                                    argument.span.clone(),
                                    state,
                                );
                            } else if !mutable {
                                loan.mutable = false;
                            }
                        }
                    }
                    for mut temporary in loans.iter().cloned() {
                        temporary.holder = None;
                        state.loans.push(temporary);
                    }
                    argument_loans.push(loans);
                }

                let receiver_return = mode.1.contains(&0);
                let persistent_receiver = if mode.0 == ReceiverMode::Mutable {
                    state.loans.retain(|loan| loan != &receiver_loan);
                    Some(self.create_loan(
                        place.clone(),
                        true,
                        through.as_deref(),
                        expression.span.clone(),
                        state,
                    ))
                } else {
                    Some(receiver_loan)
                };
                let mut returned = if receiver_return {
                    persistent_receiver.into_iter().collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                for index in mode.1.into_iter().filter(|index| *index > 0) {
                    if let Some(loans) = argument_loans.get(index - 1) {
                        returned.extend(loans.iter().cloned());
                    }
                }
                state.loans.truncate(baseline);
                returned
            }
            BodyExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.check_move_access(value, state);
                }
                self.evaluate_projected(
                    fields.iter().map(|(field, value)| (field.clone(), value)),
                    state,
                )
            }
            BodyExprKind::FieldAccess { base, field } => {
                if let Some((mut place, through)) = self.place(base, state) {
                    place.projection.push(field.clone());
                    self.access(
                        &place,
                        false,
                        through.as_deref(),
                        state,
                        expression.span.clone(),
                    );
                    state.held_at(&place.root, &place.projection)
                } else {
                    Vec::new()
                }
            }
            BodyExprKind::Index { base, index } => {
                let mut result = Vec::new();
                if let Some((mut place, through)) = self.place(base, state) {
                    place.projection.push("[*]".to_owned());
                    self.access(
                        &place,
                        false,
                        through.as_deref(),
                        state,
                        expression.span.clone(),
                    );
                    result = state.held_at(&place.root, &place.projection);
                }
                self.expression(index, state);
                result
            }
            BodyExprKind::Binary {
                operator,
                left,
                right,
            } => {
                if matches!(
                    operator,
                    BinaryOperator::Add
                        | BinaryOperator::Subtract
                        | BinaryOperator::Multiply
                        | BinaryOperator::Divide
                        | BinaryOperator::Remainder
                        | BinaryOperator::BitXor
                ) {
                    self.check_move_access(left, state);
                    self.check_move_access(right, state);
                }
                self.expression(left, state);
                self.expression(right, state);
                Vec::new()
            }
            BodyExprKind::Cast { expression, .. }
            | BodyExprKind::Negate(expression)
            | BodyExprKind::Not(expression) => {
                self.expression(expression, state);
                Vec::new()
            }
            BodyExprKind::Try(expression) => {
                self.check_move_access(expression, state);
                self.expression(expression, state);
                Vec::new()
            }
            BodyExprKind::Closure {
                parameters,
                body,
                move_capture,
            } => {
                let mut names = captures(body, parameters);
                names.retain(|name| self.mutable_locals.contains_key(name));
                if *move_capture {
                    return names
                        .iter()
                        .flat_map(|name| state.for_holder(name).cloned())
                        .collect();
                }
                let mut mutated = HashSet::new();
                collect_mutated_names(body, &mut mutated);
                let mut loans = Vec::new();
                for name in names {
                    let (place, through) = if let Some(loan) = state.for_holder(&name).next() {
                        (loan.place.clone(), Some(name.clone()))
                    } else {
                        (
                            Place {
                                root: name.clone(),
                                projection: Vec::new(),
                            },
                            None,
                        )
                    };
                    let mutable = mutated.contains(&name);
                    if mutable && !self.mutable_access_allowed(&place, through.as_deref(), state) {
                        self.report(
                            expression.span.clone(),
                            format!(
                                "error[E0596]: cannot borrow `{}` as mutable because it is not declared mutable",
                                place.root
                            ),
                        );
                        continue;
                    }
                    loans.push(self.create_loan(
                        place,
                        mutable,
                        through.as_deref(),
                        expression.span.clone(),
                        state,
                    ));
                }
                loans
            }
            BodyExprKind::Return(value) => {
                if let Some(value) = value {
                    self.check_move_access(value, state);
                    for loan in self.expression(value, state) {
                        if self.local_roots.contains(&loan.place.root) {
                            self.report(
                                loan.origin,
                                format!(
                                    "error[E0515]: cannot return reference to local variable `{}`",
                                    loan.place.root
                                ),
                            );
                        } else if let Some(expected) = &self.expected_return_lifetimes
                            && !loan.lifetimes.iter().any(|lifetime| expected.contains(lifetime))
                        {
                            self.report(
                                loan.origin.clone(),
                                "error[E0621]: explicit lifetime required to return this reference",
                            );
                        }
                    }
                }
                Vec::new()
            }
            BodyExprKind::Unit
            | BodyExprKind::Literal(_)
            | BodyExprKind::EmptySliceReference
            | BodyExprKind::Unsupported => Vec::new(),
        }
    }

    fn block(
        &mut self,
        statements: &[BodyStatement],
        tail: Option<&BodyExpr>,
        state: &mut State,
    ) -> Vec<Loan> {
        let original = state.clone();
        let original_mutability = self.mutable_locals.clone();
        let original_types = self.local_types.clone();
        let mut declarations = HashSet::new();
        for (index, statement) in statements.iter().enumerate() {
            let current = statement_live_places(statement);
            let mut future = HashSet::new();
            for next in &statements[index + 1..] {
                collect_live_statement(next, &mut future);
            }
            if let Some(tail) = tail {
                collect_live_expression(tail, &mut future);
            }
            state.loans.retain(|loan| {
                loan.holder.as_ref().is_none_or(|holder| {
                    current
                        .iter()
                        .chain(future.iter())
                        .any(|place| holder_projection_live(holder, &loan.holder_projection, place))
                })
            });
            self.statement(statement, state, &mut declarations);
        }
        let result = tail.map_or_else(Vec::new, |tail| self.expression(tail, state));
        for loan in &result {
            if declarations.contains(&loan.place.root) {
                self.report(
                    loan.origin.clone(),
                    format!(
                        "error[E0515]: cannot return reference to local variable `{}`",
                        loan.place.root
                    ),
                );
            }
        }
        for loan in &state.loans {
            let Some(holder) = loan.holder.as_deref() else {
                continue;
            };
            if declarations.contains(&loan.place.root)
                && !declarations.contains(holder)
                && original_mutability.contains_key(holder)
            {
                self.report(
                    loan.origin.clone(),
                    format!(
                        "error[E0597]: `{}` does not live long enough",
                        loan.place.root
                    ),
                );
            }
        }
        state.loans.retain(|loan| {
            loan.holder
                .as_ref()
                .is_none_or(|holder| original.all_for_holder(holder).next().is_some())
        });
        for name in declarations {
            state
                .loans
                .retain(|loan| loan.holder.as_deref() != Some(&name));
            state.loans.extend(original.all_for_holder(&name).cloned());
        }
        self.mutable_locals = original_mutability;
        self.local_types = original_types;
        result
    }

    fn evaluate_many<'a>(
        &mut self,
        expressions: impl IntoIterator<Item = &'a BodyExpr>,
        state: &mut State,
    ) -> Vec<Loan> {
        let baseline = state.loans.len();
        let mut result = Vec::new();
        for expression in expressions {
            let loans = self.expression(expression, state);
            for loan in loans {
                if !result.contains(&loan) {
                    result.push(loan.clone());
                }
                let mut temporary = loan;
                temporary.holder = None;
                if !state.loans.contains(&temporary) {
                    state.loans.push(temporary);
                }
            }
        }
        state.loans.truncate(baseline);
        result
    }

    fn evaluate_projected<'a>(
        &mut self,
        expressions: impl IntoIterator<Item = (String, &'a BodyExpr)>,
        state: &mut State,
    ) -> Vec<Loan> {
        let baseline = state.loans.len();
        let mut result = Vec::new();
        for (projection, expression) in expressions {
            for mut loan in self.expression(expression, state) {
                loan.holder_projection.insert(0, projection.clone());
                if !result.contains(&loan) {
                    result.push(loan.clone());
                }
                loan.holder = None;
                if !state.loans.contains(&loan) {
                    state.loans.push(loan);
                }
            }
        }
        state.loans.truncate(baseline);
        result
    }

    /// Join loop back edges until the set of loans at the loop header stops
    /// changing. Loans are drawn from finite source expressions, so this
    /// monotone union reaches a fixed point without an iteration cap.
    fn loop_fixed_point(
        &mut self,
        incoming: &State,
        mut transfer: impl FnMut(&mut Self, &mut State),
    ) -> State {
        let mut head = incoming.clone();
        loop {
            let mut back_edge = head.clone();
            transfer(self, &mut back_edge);
            let mut next = incoming.clone();
            next.merge(back_edge);
            if next.same_loans(&head) {
                return next;
            }
            head = next;
        }
    }

    fn statement(
        &mut self,
        statement: &BodyStatement,
        state: &mut State,
        declarations: &mut HashSet<String>,
    ) {
        match statement {
            BodyStatement::Let {
                name,
                mutable,
                initializer,
                type_text,
                ..
            } => {
                let binding_type = type_text
                    .as_deref()
                    .map(|type_text| type_from_syntax(type_text, self.struct_signatures))
                    .or_else(|| initializer.as_ref().map(|initializer| self.infer_local_type(initializer)))
                    .unwrap_or(Ty::Unknown);
                if let Some(name) = name
                    && (declarations.contains(name) || self.mutable_locals.contains_key(name))
                {
                    let mut initializer_names = HashSet::new();
                    if let Some(initializer) = initializer {
                        expression_names(initializer, &mut initializer_names);
                    }
                    if !initializer_names.contains(name) {
                        state
                            .loans
                            .retain(|loan| loan.holder.as_deref() != Some(name));
                    }
                }
                let moved_reference = initializer
                    .as_ref()
                    .and_then(simple_path)
                    .filter(|source| state.for_holder(source).any(|loan| loan.mutable));
                if let Some(initializer) = initializer {
                    self.check_move_access(initializer, state);
                }
                let loans = initializer
                    .as_ref()
                    .map_or_else(Vec::new, |initializer| self.expression(initializer, state));
                if let Some(name) = name {
                    declarations.insert(name.clone());
                    state.replace_holder(name, loans);
                    self.mutable_locals.insert(name.clone(), *mutable);
                    self.local_types.insert(name.clone(), binding_type);
                    if let Some(source) = moved_reference {
                        if source != *name {
                            state
                                .loans
                                .retain(|loan| loan.holder.as_deref() != Some(&source));
                        }
                    }
                }
            }
            BodyStatement::LetTuple {
                names, initializer, ..
            } => {
                let initializer_type = self.infer_local_type(initializer);
                self.check_move_access(initializer, state);
                self.expression(initializer, state);
                for (index, name) in names.iter().enumerate() {
                    declarations.insert(name.clone());
                    state.replace_holder(name, Vec::new());
                    self.mutable_locals.insert(name.clone(), false);
                    let binding_type = match &initializer_type {
                        Ty::Tuple(types) => types.get(index).cloned().unwrap_or(Ty::Unknown),
                        _ => Ty::Unknown,
                    };
                    self.local_types.insert(name.clone(), binding_type);
                }
            }
            BodyStatement::Assign { name, value, span } => {
                let moved_reference = if is_direct_binding(name) {
                    simple_path(value)
                        .filter(|source| state.for_holder(source).any(|loan| loan.mutable))
                } else {
                    None
                };
                if is_direct_binding(name) {
                    let mut rhs_names = HashSet::new();
                    expression_names(value, &mut rhs_names);
                    if !rhs_names.contains(name) {
                        // Replacing a reference ends the old value's loan at
                        // this assignment when the right hand side does not
                        // read that value. This is also the kill operation
                        // needed for loop back-edge dataflow.
                        state
                            .loans
                            .retain(|loan| loan.holder.as_deref() != Some(name));
                    }
                }
                self.check_move_access(value, state);
                let aliases = self.expression(value, state);
                let assignment = self.assignment_place(name, state);
                if let Some((place, through)) = assignment.as_ref() {
                    if let Some(holder) = through {
                        self.access(
                            &Place {
                                root: holder.clone(),
                                projection: Vec::new(),
                            },
                            true,
                            None,
                            state,
                            span.clone(),
                        );
                    }
                    self.access(place, true, through.as_deref(), state, span.clone());
                }
                if is_direct_binding(name) {
                    state.replace_holder(name, aliases);
                    if let Some(source) = moved_reference {
                        if source != *name {
                            state
                                .loans
                                .retain(|loan| loan.holder.as_deref() != Some(&source));
                        }
                    }
                } else if let Some((place, _)) = assignment
                    && !place.projection.is_empty()
                    && !matches!(self.local_types.get(&place.root), Some(Ty::Reference(_, _)))
                {
                    state.replace_held_at(&place.root, &place.projection, aliases);
                }
            }
            BodyStatement::Expression { expression, .. } => {
                self.expression(expression, state);
            }
            BodyStatement::While {
                condition, body, ..
            } => {
                *state = self.loop_fixed_point(state, |checker, loop_state| {
                    checker.expression(condition, loop_state);
                    checker.expression(body, loop_state);
                });
            }
            BodyStatement::Loop { body, .. } => {
                *state = self.loop_fixed_point(state, |checker, loop_state| {
                    checker.expression(body, loop_state);
                });
            }
            BodyStatement::ForRange {
                name,
                start,
                end,
                body,
                ..
            } => {
                let binding_type = self.infer_local_type(start);
                self.expression(start, state);
                self.expression(end, state);
                declarations.insert(name.clone());
                self.mutable_locals.insert(name.clone(), false);
                self.local_types.insert(name.clone(), binding_type);
                *state = self.loop_fixed_point(state, |checker, loop_state| {
                    checker.expression(body, loop_state);
                });
            }
            BodyStatement::ForEach {
                names,
                iterator,
                body,
                span,
                ..
            } => {
                let original = state.clone();
                let original_mutability = self.mutable_locals.clone();
                let original_types = self.local_types.clone();
                let iterator_type = self.infer_local_type(iterator);
                let (item_type, borrowed_item) = match &iterator_type {
                    Ty::Reference(inner, _) => match inner.as_ref() {
                        Ty::Slice(element) | Ty::Vector(element) => {
                            (Some((**element).clone()), true)
                        }
                        _ => (None, false),
                    },
                    Ty::Slice(element) | Ty::Vector(element) => {
                        (Some((**element).clone()), false)
                    }
                    Ty::Iterator(element) => (Some((**element).clone()), false),
                    _ => (None, false),
                };
                self.check_move_access(iterator, state);
                let mut iterator_loans = self.expression(iterator, state);
                let iterator_holder = format!("\0iterator:{}", span.start);
                for loan in &mut iterator_loans {
                    loan.holder = Some(iterator_holder.clone());
                    loan.holder_projection.clear();
                    state.loans.push(loan.clone());
                }
                declarations.extend(names.iter().cloned());
                let binding_types = match &item_type {
                    Some(Ty::Tuple(fields)) if fields.len() == names.len() => fields.clone(),
                    Some(item) if names.len() == 1 => vec![item.clone()],
                    _ => vec![Ty::Unknown; names.len()],
                };
                for (index, name) in names.iter().enumerate() {
                    let mut binding_type = binding_types[index].clone();
                    let has_reference_loan = borrowed_item
                        || matches!(binding_type, Ty::Reference(_, _));
                    if borrowed_item {
                        binding_type = Ty::Reference(Box::new(binding_type), false);
                    }
                    self.mutable_locals.insert(name.clone(), false);
                    self.local_types.insert(name.clone(), binding_type);
                    let mut item_loans = Vec::new();
                    if has_reference_loan {
                        for iterator_loan in &iterator_loans {
                            let mut item_loan = iterator_loan.clone();
                            item_loan.holder = None;
                            item_loan.holder_projection.clear();
                            item_loan.parent = Some(iterator_holder.clone());
                            item_loan.place.projection.push("[*]".to_owned());
                            if names.len() > 1 {
                                item_loan.place.projection.push(index.to_string());
                            }
                            item_loans.push(item_loan);
                        }
                    }
                    state.replace_holder(name, item_loans);
                }
                *state = self.loop_fixed_point(state, |checker, loop_state| {
                    checker.expression(body, loop_state);
                });
                for name in names {
                    for loan in state.loans.iter().filter(|loan| {
                        loan.place.root == *name
                            && loan.holder.as_deref().is_some_and(|holder| !names.contains(&holder.to_owned()))
                    }) {
                        self.report(
                            loan.origin.clone(),
                            format!("error[E0597]: `{name}` does not live long enough"),
                        );
                    }
                    state
                        .loans
                        .retain(|loan| loan.holder.as_deref() != Some(name));
                    state.loans.extend(original.all_for_holder(name).cloned());
                }
                state.loans.retain(|loan| loan.holder.as_deref() != Some(&iterator_holder));
                self.mutable_locals = original_mutability;
                self.local_types = original_types;
            }
            BodyStatement::MacroInvocation { arguments, .. } => {
                for argument in arguments {
                    self.expression(argument, state);
                }
            }
            BodyStatement::Break { .. }
            | BodyStatement::Continue { .. }
            | BodyStatement::Unsupported => {}
        }
    }

    fn place(&self, expression: &BodyExpr, state: &State) -> Option<(Place, Option<String>)> {
        match &expression.kind {
            BodyExprKind::Path(name) => {
                if matches!(self.local_types.get(name), Some(Ty::Reference(_, _)))
                    && let Some(loan) = state.for_holder(name).next()
                {
                    Some((loan.place.clone(), Some(name.clone())))
                } else {
                    Some((
                        Place {
                            root: name.clone(),
                            projection: Vec::new(),
                        },
                        None,
                    ))
                }
            }
            BodyExprKind::Parenthesized(inner) => self.place(inner, state),
            BodyExprKind::FieldAccess { base, field } => {
                let (mut place, through) = self.place(base, state)?;
                place.projection.push(field.clone());
                Some((place, through))
            }
            BodyExprKind::Index { base, .. } => {
                let (mut place, through) = self.place(base, state)?;
                place.projection.push("[*]".to_owned());
                Some((place, through))
            }
            BodyExprKind::Dereference(inner) => {
                let aliases = self.expression_place_alias(inner, state);
                aliases
                    .into_iter()
                    .next()
                    .map(|loan| (loan.place, loan.holder))
            }
            _ => None,
        }
    }

    fn place_for_borrow(
        &self,
        expression: &BodyExpr,
        state: &State,
    ) -> Option<(Place, Option<String>)> {
        match &expression.kind {
            BodyExprKind::Path(name) => Some((
                Place {
                    root: name.clone(),
                    projection: Vec::new(),
                },
                None,
            )),
            BodyExprKind::Parenthesized(inner) => self.place_for_borrow(inner, state),
            _ => self.place(expression, state),
        }
    }

    fn expression_place_alias(&self, expression: &BodyExpr, state: &State) -> Vec<Loan> {
        match &expression.kind {
            BodyExprKind::Path(name) => state.for_holder(name).cloned().collect(),
            BodyExprKind::Parenthesized(inner) => self.expression_place_alias(inner, state),
            _ => Vec::new(),
        }
    }

    fn assignment_place(&self, text: &str, state: &State) -> Option<(Place, Option<String>)> {
        let (parsed, _) = place_from_text(text)?;
        if parsed.projection.is_empty() {
            return Some((parsed, None));
        }
        let holder = parsed.root.clone();
        let holder_place = state
            .for_holder(&holder)
            .next()
            .map(|loan| loan.place.clone());
        if let Some(mut place) = holder_place {
            place.projection.extend(
                parsed
                    .projection
                    .into_iter()
                    .filter(|projection| projection != "[*deref*]"),
            );
            Some((place, Some(holder.clone())))
        } else {
            Some((parsed, None))
        }
    }
}

fn returned_borrowed_parameters(function: &FunctionItem) -> Vec<usize> {
    let Some(return_type) = function.return_type.as_deref() else {
        return Vec::new();
    };
    let return_lifetimes = named_lifetimes(return_type);
    let return_has_reference = return_type.contains('&');
    let reference_parameters = function
        .parameters
        .iter()
        .enumerate()
        .filter(|(_, parameter)| parameter.type_text.trim_start().starts_with('&'))
        .collect::<Vec<_>>();
    let by_lifetime = function
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(index, parameter)| {
            named_lifetimes(&parameter.type_text)
                .iter()
                .any(|lifetime| return_lifetimes.contains(lifetime))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if !by_lifetime.is_empty() {
        by_lifetime
    } else if return_has_reference
        && function.scope.iter().any(|scope| scope.starts_with("impl:"))
        && reference_parameters.first().is_some_and(|(index, _)| *index == 0)
    {
        vec![0]
    } else if return_has_reference && reference_parameters.len() == 1 {
        vec![reference_parameters[0].0]
    } else {
        Vec::new()
    }
}

fn named_lifetimes(text: &str) -> HashSet<String> {
    let mut lifetimes = HashSet::new();
    let mut rest = text;
    while let Some(apostrophe) = rest.find('\'') {
        rest = &rest[apostrophe + 1..];
        let length = rest
            .chars()
            .take_while(|character| character.is_alphanumeric() || *character == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        if length == 0 {
            continue;
        }
        let lifetime = &rest[..length];
        if lifetime != "_" {
            lifetimes.insert(lifetime.to_owned());
        }
        rest = &rest[length..];
    }
    lifetimes
}

fn parameter_lifetimes(type_text: &str, name: &str) -> Vec<String> {
    let mut lifetimes = named_lifetimes(type_text).into_iter().collect::<Vec<_>>();
    if lifetimes.is_empty() {
        lifetimes.push(format!("\0input:{name}"));
    }
    lifetimes.sort();
    lifetimes
}

fn return_lifetime_contract(function: &FunctionItem) -> Option<Vec<String>> {
    let return_type = function.return_type.as_deref()?;
    let mut lifetimes = named_lifetimes(return_type).into_iter().collect::<Vec<_>>();
    if !lifetimes.is_empty() {
        lifetimes.sort();
        return Some(lifetimes);
    }
    if !return_type.contains('&') {
        return None;
    }
    let reference_parameters = function
        .parameters
        .iter()
        .filter(|parameter| parameter.type_text.trim_start().starts_with('&'))
        .collect::<Vec<_>>();
    if function.scope.iter().any(|scope| scope.starts_with("impl:"))
        && reference_parameters
            .first()
            .is_some_and(|parameter| parameter.name.as_deref() == Some("self"))
    {
        return Some(parameter_lifetimes(
            &reference_parameters[0].type_text,
            "self",
        ));
    }
    if reference_parameters.len() == 1 {
        return Some(parameter_lifetimes(
            &reference_parameters[0].type_text,
            reference_parameters[0].name.as_deref().unwrap_or("_"),
        ));
    }
    Some(Vec::new())
}

fn is_mutable_reference_type(type_text: &str) -> bool {
    let Some(mut referent) = type_text.trim_start().strip_prefix('&') else {
        return false;
    };
    referent = referent.trim_start();
    if let Some(lifetime) = referent.strip_prefix('\'') {
        let lifetime_length = lifetime
            .chars()
            .take_while(|character| character.is_alphanumeric() || *character == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        referent = lifetime[lifetime_length..].trim_start();
    }
    referent.starts_with("mut ")
}

fn place_from_text(text: &str) -> Option<(Place, Option<String>)> {
    let mut rest = text.trim();
    let mut projection = Vec::new();
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('*') {
            projection.push("[*deref*]".to_owned());
            rest = after;
        } else if let Some(after) = rest.strip_prefix('(') {
            rest = after;
        } else {
            break;
        }
    }
    rest = rest.trim_start();
    let root_len = rest
        .char_indices()
        .take_while(|(_, character)| {
            character.is_ascii_alphanumeric() || *character == '_'
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    let root = &rest[..root_len];
    if root.is_empty() {
        return None;
    }
    rest = &rest[root_len..];
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('.') {
            let field_len = after
                .char_indices()
                .take_while(|(_, character)| {
                    character.is_ascii_alphanumeric() || *character == '_'
                })
                .map(|(index, character)| index + character.len_utf8())
                .last()?;
            projection.push(after[..field_len].to_owned());
            rest = &after[field_len..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let mut depth = 1usize;
            let mut close = None;
            for (index, character) in after.char_indices() {
                match character {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(index + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = &after[close?..];
            projection.push("[*]".to_owned());
        } else if let Some(after) = rest.strip_prefix(')') {
            rest = after;
        } else {
            break;
        }
    }
    Some((Place { root: root.to_owned(), projection }, None))
}

fn is_direct_binding(text: &str) -> bool {
    !text.trim().is_empty() && !text.contains(['.', '[', ']', '*', '(', ')'])
}

fn simple_path(expression: &BodyExpr) -> Option<String> {
    match &expression.kind {
        BodyExprKind::Path(name) => Some(name.clone()),
        BodyExprKind::Parenthesized(inner) => simple_path(inner),
        _ => None,
    }
}

fn expression_names(expression: &BodyExpr, names: &mut HashSet<String>) {
    match &expression.kind {
        BodyExprKind::Path(name) => {
            names.insert(name.clone());
        }
        BodyExprKind::Reference { expression, .. }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Dereference(expression)
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression) => expression_names(expression, names),
        BodyExprKind::Call {
            callee, arguments, ..
        } => {
            if !callee.contains("::") {
                names.insert(callee.clone());
            }
            for argument in arguments {
                expression_names(argument, names);
            }
        }
        BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => {
            for argument in arguments {
                expression_names(argument, names);
            }
        }
        BodyExprKind::ArrayRepeat { element, .. } => expression_names(element, names),
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            expression_names(receiver, names);
            for argument in arguments {
                expression_names(argument, names);
            }
        }
        BodyExprKind::Closure { parameters, body, .. } => {
            expression_names(body, names);
            for parameter in parameters {
                match parameter {
                    ClosureParameter::Identifier(name) => {
                        names.remove(name);
                    }
                    ClosureParameter::Tuple(items) => {
                        for name in items {
                            names.remove(name);
                        }
                    }
                }
            }
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                expression_names(value, names);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => expression_names(base, names),
        BodyExprKind::Index { base, index } => {
            expression_names(base, names);
            expression_names(index, names);
        }
        BodyExprKind::Binary { left, right, .. } => {
            expression_names(left, names);
            expression_names(right, names);
        }
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                statement_names_into(statement, names);
            }
            if let Some(tail) = tail {
                expression_names(tail, names);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expression_names(condition, names);
            expression_names(then_branch, names);
            if let Some(otherwise) = else_branch {
                expression_names(otherwise, names);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            expression_names(scrutinee, names);
            for arm in arms {
                expression_names(&arm.body, names);
            }
        }
        BodyExprKind::Return(value) => {
            if let Some(value) = value {
                expression_names(value, names);
            }
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Unsupported => {}
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                expression_names(argument, names);
            }
            if let Some(format) = &macro_expression.format_string {
                format_capture_names(format, names);
            }
        }
    }
}

fn statement_live_places(statement: &BodyStatement) -> HashSet<Place> {
    let mut places = HashSet::new();
    collect_live_statement(statement, &mut places);
    places
}

fn holder_projection_live(holder: &str, projection: &[String], live: &Place) -> bool {
    holder == live.root
        && projection
            .iter()
            .zip(&live.projection)
            .all(|(held, used)| held == used || held == "[*]" || used == "[*]")
}

fn collect_live_statement(statement: &BodyStatement, places: &mut HashSet<Place>) {
    match statement {
        BodyStatement::Let { initializer, .. } => {
            if let Some(initializer) = initializer {
                collect_live_expression(initializer, places);
            }
        }
        BodyStatement::LetTuple { initializer, .. } => {
            collect_live_expression(initializer, places);
        }
        BodyStatement::Assign { name, value, .. } => {
            if let Some((place, _)) = place_from_text(name) {
                places.insert(place);
            }
            collect_live_expression(value, places);
        }
        BodyStatement::While { condition, body, .. } => {
            collect_live_expression(condition, places);
            collect_live_expression(body, places);
        }
        BodyStatement::Loop { body, .. } => collect_live_expression(body, places),
        BodyStatement::ForRange {
            start,
            end,
            body,
            ..
        } => {
            collect_live_expression(start, places);
            collect_live_expression(end, places);
            collect_live_expression(body, places);
        }
        BodyStatement::ForEach { iterator, body, .. } => {
            collect_live_expression(iterator, places);
            collect_live_expression(body, places);
        }
        BodyStatement::MacroInvocation {
            arguments,
            format_string,
            ..
        } => {
            for argument in arguments {
                collect_live_expression(argument, places);
            }
            if let Some(format) = format_string {
                let mut names = HashSet::new();
                format_capture_names(format, &mut names);
                places.extend(names.into_iter().map(|root| Place {
                    root,
                    projection: Vec::new(),
                }));
            }
        }
        BodyStatement::Expression { expression, .. } => {
            collect_live_expression(expression, places);
        }
        BodyStatement::Break { .. }
        | BodyStatement::Continue { .. }
        | BodyStatement::Unsupported => {}
    }
}

fn collect_live_expression(expression: &BodyExpr, places: &mut HashSet<Place>) {
    match &expression.kind {
        BodyExprKind::Path(name) => {
            places.insert(Place {
                root: name.clone(),
                projection: Vec::new(),
            });
        }
        BodyExprKind::FieldAccess { .. } | BodyExprKind::Index { .. }
            if place_from_expression(expression).is_some() =>
        {
            let place = place_from_expression(expression).expect("checked place");
            // A partial aggregate use keeps the aggregate temporary live as a
            // whole under rustc's current MIR liveness rules. The projection
            // is still retained for reads and move tracking.
            places.insert(Place {
                root: place.root.clone(),
                projection: Vec::new(),
            });
            places.insert(place);
            if let BodyExprKind::Index { index, .. } = &expression.kind {
                collect_live_expression(index, places);
            }
        }
        BodyExprKind::Reference { expression, .. }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Dereference(expression)
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression) => {
            collect_live_expression(expression, places);
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } => {
            if !callee.contains("::") {
                places.insert(Place {
                    root: callee.clone(),
                    projection: Vec::new(),
                });
            }
            for argument in arguments {
                collect_live_expression(argument, places);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            collect_live_expression(receiver, places);
            for argument in arguments {
                collect_live_expression(argument, places);
            }
        }
        BodyExprKind::Closure { body, .. } => collect_live_expression(body, places),
        BodyExprKind::Tuple(expressions)
        | BodyExprKind::ArrayLiteral(expressions)
        | BodyExprKind::FixedArrayLiteral(expressions) => {
            for expression in expressions {
                collect_live_expression(expression, places);
            }
        }
        BodyExprKind::ArrayRepeat { element, .. } => collect_live_expression(element, places),
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_live_expression(value, places);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => collect_live_expression(base, places),
        BodyExprKind::Index { base, index } => {
            collect_live_expression(base, places);
            collect_live_expression(index, places);
        }
        BodyExprKind::Binary { left, right, .. } => {
            collect_live_expression(left, places);
            collect_live_expression(right, places);
        }
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                collect_live_statement(statement, places);
            }
            if let Some(tail) = tail {
                collect_live_expression(tail, places);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_live_expression(condition, places);
            collect_live_expression(then_branch, places);
            if let Some(otherwise) = else_branch {
                collect_live_expression(otherwise, places);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            collect_live_expression(scrutinee, places);
            for arm in arms {
                collect_live_expression(&arm.body, places);
            }
        }
        BodyExprKind::Return(value) => {
            if let Some(value) = value {
                collect_live_expression(value, places);
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                collect_live_expression(argument, places);
            }
            if let Some(format) = &macro_expression.format_string {
                let mut names = HashSet::new();
                format_capture_names(format, &mut names);
                places.extend(names.into_iter().map(|root| Place {
                    root,
                    projection: Vec::new(),
                }));
            }
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Unsupported => {}
    }
}

fn statement_names_into(statement: &BodyStatement, names: &mut HashSet<String>) {
    match statement {
        BodyStatement::Let {
            name, initializer, ..
        } => {
            if let Some(initializer) = initializer {
                expression_names(initializer, names);
            }
            if let Some(name) = name {
                names.insert(name.clone());
            }
        }
        BodyStatement::LetTuple {
            names: bindings,
            initializer,
            ..
        } => {
            expression_names(initializer, names);
            names.extend(bindings.iter().cloned());
        }
        BodyStatement::Assign { name, value, .. } => {
            if let Some((place, _)) = place_from_text(name) {
                names.insert(place.root);
            }
            expression_names(value, names);
        }
        BodyStatement::While {
            condition, body, ..
        } => {
            expression_names(condition, names);
            expression_names(body, names);
        }
        BodyStatement::Loop { body, .. } => expression_names(body, names),
        BodyStatement::ForRange {
            name,
            start,
            end,
            body,
            ..
        } => {
            names.insert(name.clone());
            expression_names(start, names);
            expression_names(end, names);
            expression_names(body, names);
        }
        BodyStatement::ForEach {
            names: bindings,
            iterator,
            body,
            ..
        } => {
            names.extend(bindings.iter().cloned());
            expression_names(iterator, names);
            expression_names(body, names);
        }
        BodyStatement::MacroInvocation {
            arguments,
            format_string,
            ..
        } => {
            for argument in arguments {
                expression_names(argument, names);
            }
            if let Some(format) = format_string {
                format_capture_names(format, names);
            }
        }
        BodyStatement::Expression { expression, .. } => expression_names(expression, names),
        BodyStatement::Break { .. }
        | BodyStatement::Continue { .. }
        | BodyStatement::Unsupported => {}
    }
}

fn format_capture_names(format: &str, names: &mut HashSet<String>) {
    let mut cursor = 0;
    while let Some(open_relative) = format[cursor..].find('{') {
        let open = cursor + open_relative;
        if format[open..].starts_with("{{") {
            cursor = open + 2;
            continue;
        }
        let Some(close_relative) = format[open + 1..].find('}') else {
            break;
        };
        let close = open + 1 + close_relative;
        let capture = format[open + 1..close]
            .split([':', '!'])
            .next()
            .unwrap_or_default()
            .trim();
        if !capture.is_empty()
            && capture
                .chars()
                .all(|character| character == '_' || character.is_alphanumeric())
        {
            names.insert(capture.to_owned());
        }
        cursor = close + 1;
    }
}

fn pattern_bindings(pattern: &MatchPattern) -> Vec<String> {
    match pattern {
        MatchPattern::Binding(name) => vec![name.clone()],
        MatchPattern::Or(patterns) => {
            let mut bindings = Vec::new();
            for pattern in patterns {
                for name in pattern_bindings(pattern) {
                    if !bindings.contains(&name) {
                        bindings.push(name);
                    }
                }
            }
            bindings
        }
        _ => Vec::new(),
    }
}

fn collect_bindings(expression: &BodyExpr, bindings: &mut HashSet<String>) {
    match &expression.kind {
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        name, initializer, ..
                    } => {
                        bindings.extend(name.iter().cloned());
                        if let Some(initializer) = initializer {
                            collect_bindings(initializer, bindings);
                        }
                    }
                    BodyStatement::LetTuple {
                        names, initializer, ..
                    } => {
                        bindings.extend(names.iter().cloned());
                        collect_bindings(initializer, bindings);
                    }
                    BodyStatement::While { condition, body, .. } => {
                        collect_bindings(condition, bindings);
                        collect_bindings(body, bindings);
                    }
                    BodyStatement::Loop { body, .. } => collect_bindings(body, bindings),
                    BodyStatement::ForRange {
                        name,
                        start,
                        end,
                        body,
                        ..
                    } => {
                        bindings.insert(name.clone());
                        collect_bindings(start, bindings);
                        collect_bindings(end, bindings);
                        collect_bindings(body, bindings);
                    }
                    BodyStatement::ForEach {
                        names,
                        iterator,
                        body,
                        ..
                    } => {
                        bindings.extend(names.iter().cloned());
                        collect_bindings(iterator, bindings);
                        collect_bindings(body, bindings);
                    }
                    BodyStatement::Assign { value, .. }
                    | BodyStatement::Expression {
                        expression: value, ..
                    } => collect_bindings(value, bindings),
                    BodyStatement::MacroInvocation { arguments, .. } => {
                        for argument in arguments {
                            collect_bindings(argument, bindings);
                        }
                    }
                    BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
                    | BodyStatement::Unsupported => {}
                }
            }
            if let Some(tail) = tail {
                collect_bindings(tail, bindings);
            }
        }
        BodyExprKind::Reference { expression, .. }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Dereference(expression)
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression) => collect_bindings(expression, bindings),
        BodyExprKind::ArrayRepeat { element, .. } => collect_bindings(element, bindings),
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => {
            for argument in arguments {
                collect_bindings(argument, bindings);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            collect_bindings(receiver, bindings);
            for argument in arguments {
                collect_bindings(argument, bindings);
            }
        }
        BodyExprKind::Closure { body, .. } => collect_bindings(body, bindings),
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                collect_bindings(argument, bindings);
            }
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_bindings(value, bindings);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => collect_bindings(base, bindings),
        BodyExprKind::Index { base, index } => {
            collect_bindings(base, bindings);
            collect_bindings(index, bindings);
        }
        BodyExprKind::Binary { left, right, .. } => {
            collect_bindings(left, bindings);
            collect_bindings(right, bindings);
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_bindings(condition, bindings);
            collect_bindings(then_branch, bindings);
            if let Some(otherwise) = else_branch {
                collect_bindings(otherwise, bindings);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            collect_bindings(scrutinee, bindings);
            for arm in arms {
                if let MatchPattern::Binding(name) = &arm.pattern {
                    bindings.insert(name.clone());
                }
                collect_bindings(&arm.body, bindings);
            }
        }
        BodyExprKind::Return(value) => {
            if let Some(value) = value {
                collect_bindings(value, bindings);
            }
        }
        BodyExprKind::Path(_)
        | BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Unsupported => {}
    }
}

fn collect_mutated_names(expression: &BodyExpr, mutated: &mut HashSet<String>) {
    match &expression.kind {
        BodyExprKind::Reference {
            mutable: true,
            expression,
        } => {
            if let Some(place) = place_from_expression(expression) {
                mutated.insert(place.root);
            }
            collect_mutated_names(expression, mutated);
        }
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Assign { name, value, .. } => {
                        if let Some((place, _)) = place_from_text(name) {
                            mutated.insert(place.root);
                        }
                        collect_mutated_names(value, mutated);
                    }
                    BodyStatement::Let { initializer, .. } => {
                        if let Some(initializer) = initializer {
                            collect_mutated_names(initializer, mutated);
                        }
                    }
                    BodyStatement::LetTuple { initializer, .. } => {
                        collect_mutated_names(initializer, mutated);
                    }
                    BodyStatement::While { condition, body, .. } => {
                        collect_mutated_names(condition, mutated);
                        collect_mutated_names(body, mutated);
                    }
                    BodyStatement::Loop { body, .. } => collect_mutated_names(body, mutated),
                    BodyStatement::ForRange { start, end, body, .. } => {
                        collect_mutated_names(start, mutated);
                        collect_mutated_names(end, mutated);
                        collect_mutated_names(body, mutated);
                    }
                    BodyStatement::ForEach { iterator, body, .. } => {
                        collect_mutated_names(iterator, mutated);
                        collect_mutated_names(body, mutated);
                    }
                    BodyStatement::MacroInvocation { arguments, .. } => {
                        for argument in arguments {
                            collect_mutated_names(argument, mutated);
                        }
                    }
                    BodyStatement::Expression { expression, .. } => {
                        collect_mutated_names(expression, mutated);
                    }
                    BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
                    | BodyStatement::Unsupported => {}
                }
            }
            if let Some(tail) = tail {
                collect_mutated_names(tail, mutated);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            method,
        } => {
            if matches!(
                method.as_str(),
                "append"
                    | "clear"
                    | "extend"
                    | "get_mut"
                    | "insert"
                    | "iter_mut"
                    | "pop"
                    | "push"
                    | "push_str"
                    | "remove"
                    | "reserve"
                    | "retain"
                    | "reverse"
                    | "sort"
                    | "truncate"
                    | "values_mut"
            ) && let Some(place) = place_from_expression(receiver)
            {
                mutated.insert(place.root);
            }
            collect_mutated_names(receiver, mutated);
            for argument in arguments {
                collect_mutated_names(argument, mutated);
            }
        }
        BodyExprKind::Reference {
            expression,
            mutable: false,
        }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Dereference(expression)
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression) => collect_mutated_names(expression, mutated),
        BodyExprKind::ArrayRepeat { element, .. } => collect_mutated_names(element, mutated),
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => {
            for argument in arguments {
                collect_mutated_names(argument, mutated);
            }
        }
        BodyExprKind::Closure { body, .. } => collect_mutated_names(body, mutated),
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                collect_mutated_names(argument, mutated);
            }
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_mutated_names(value, mutated);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => collect_mutated_names(base, mutated),
        BodyExprKind::Index { base, index } => {
            collect_mutated_names(base, mutated);
            collect_mutated_names(index, mutated);
        }
        BodyExprKind::Binary { left, right, .. } => {
            collect_mutated_names(left, mutated);
            collect_mutated_names(right, mutated);
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_mutated_names(condition, mutated);
            collect_mutated_names(then_branch, mutated);
            if let Some(otherwise) = else_branch {
                collect_mutated_names(otherwise, mutated);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            collect_mutated_names(scrutinee, mutated);
            for arm in arms {
                collect_mutated_names(&arm.body, mutated);
            }
        }
        BodyExprKind::Return(value) => {
            if let Some(value) = value {
                collect_mutated_names(value, mutated);
            }
        }
        BodyExprKind::Path(_)
        | BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Unsupported => {}
    }
}

fn place_from_expression(expression: &BodyExpr) -> Option<Place> {
    match &expression.kind {
        BodyExprKind::Path(name) => Some(Place {
            root: name.clone(),
            projection: Vec::new(),
        }),
        BodyExprKind::Parenthesized(inner) => place_from_expression(inner),
        BodyExprKind::FieldAccess { base, field } => {
            let mut place = place_from_expression(base)?;
            place.projection.push(field.clone());
            Some(place)
        }
        BodyExprKind::Index { base, .. } => {
            let mut place = place_from_expression(base)?;
            place.projection.push("[*]".to_owned());
            Some(place)
        }
        BodyExprKind::Dereference(inner) => match &inner.kind {
            BodyExprKind::Path(name) => Some(Place {
                root: name.clone(),
                projection: vec!["[*deref*]".to_owned()],
            }),
            _ => None,
        },
        _ => None,
    }
}
