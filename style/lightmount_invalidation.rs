/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Lightmount-facing selector invalidation summaries.
//!
//! This module exposes a small, read-only view over Stylo's existing selector
//! invalidation maps. It lets Lightmount consume selector dependency truth
//! without adopting Servo's full restyle lifecycle.

use std::cell::RefCell;
use std::collections::HashSet;
use std::hash::Hash;

use crate::context::QuirksMode;
use crate::derives::*;
use crate::dom::{TDocument, TElement, TNode};
use crate::invalidation::element::element_wrapper::ElementWrapper;
use crate::invalidation::element::invalidation_map::{
    AdditionalRelativeSelectorInvalidationMap, Dependency, DependencyInvalidationKind,
    InvalidationMap, NormalDependencyInvalidationKind, RelativeDependencyInvalidationKind,
    ScopeDependencyInvalidationKind, StateDependency,
};
use crate::invalidation::element::invalidator::{
    any_next_has_scope_in_negation, note_scope_dependency_force_at_subject,
    DescendantInvalidationLists, Invalidation, InvalidationProcessor, InvalidationResult,
    InvalidationVector, SiblingTraversalMap,
};
use crate::invalidation::element::relative_selector::RelativeSelectorInvalidator;
use crate::invalidation::element::state_and_attributes::check_dependency;
use crate::selector_map::SelectorMap;
use crate::selector_parser::SnapshotMap;
use crate::stylist::{CascadeData, Stylist};
use crate::values::AtomIdent;
use crate::{Atom, LocalName};
use dom::ElementState;
use indexmap::{IndexMap, IndexSet};
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches,
};
use selectors::OpaqueElement;
use servo_arc::Arc as ServoArc;

/// Sibling-sensitive selector dependencies exposed for Lightmount's style
/// invalidation summary.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountSiblingInvalidationSummary {
    /// Class tokens whose mutation can affect sibling selector invalidation.
    pub class_dependencies: Vec<Atom>,
    /// IDs whose mutation can affect sibling selector invalidation.
    pub id_dependencies: Vec<Atom>,
    /// Attribute local names whose mutation can affect sibling selector
    /// invalidation.
    pub attribute_dependencies: Vec<LocalName>,
    /// Whether focus-like state changes can affect sibling selector
    /// invalidation.
    pub focus_dependency: bool,
    /// Whether :target state changes can affect sibling selector invalidation.
    pub target_dependency: bool,
    /// Whether there are sibling-sensitive dependencies that are not covered by
    /// the class/id/attribute/focus/target keys above.
    pub unknown_dependency: bool,
}

impl LightmountSiblingInvalidationSummary {
    /// Returns whether any sibling-sensitive dependency was found.
    #[inline]
    pub fn has_any_dependency(&self) -> bool {
        !self.class_dependencies.is_empty()
            || !self.id_dependencies.is_empty()
            || !self.attribute_dependencies.is_empty()
            || self.focus_dependency
            || self.target_dependency
            || self.unknown_dependency
    }

    fn note_state_dependency(&mut self, state: ElementState) {
        let mut known = false;
        if state
            .intersects(ElementState::FOCUS | ElementState::FOCUSRING | ElementState::FOCUS_WITHIN)
        {
            self.focus_dependency = true;
            known = true;
        }
        if state.intersects(ElementState::URLTARGET) {
            self.target_dependency = true;
            known = true;
        }
        if !known {
            self.unknown_dependency = true;
        }
    }

    fn note_unknown_dependency(&mut self) {
        self.unknown_dependency = true;
    }

    /// Merge another sibling invalidation summary into this one.
    pub fn extend(&mut self, other: LightmountSiblingInvalidationSummary) {
        self.class_dependencies.extend(other.class_dependencies);
        self.id_dependencies.extend(other.id_dependencies);
        self.attribute_dependencies
            .extend(other.attribute_dependencies);
        self.focus_dependency |= other.focus_dependency;
        self.target_dependency |= other.target_dependency;
        self.unknown_dependency |= other.unknown_dependency;
    }

    /// Returns whether mutating this class token can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_class_dependency(&self, class: &Atom) -> bool {
        self.class_dependencies
            .iter()
            .any(|candidate| candidate == class)
    }

    /// Returns whether mutating this id can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_id_dependency(&self, id: &Atom) -> bool {
        self.id_dependencies.iter().any(|candidate| candidate == id)
    }

    /// Returns whether mutating this attribute can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_attribute_dependency(&self, attribute: &LocalName) -> bool {
        self.attribute_dependencies
            .iter()
            .any(|candidate| candidate == attribute)
    }
}

/// Lightmount-facing invalidation dependency kind extracted from Stylo's
/// selector invalidation maps.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LightmountDependencyKind {
    /// The changed element itself can be invalidated.
    Element,
    /// The changed element and descendants can be invalidated.
    ElementAndDescendants,
    /// Descendants of the changed element can be invalidated.
    Descendants,
    /// Following siblings of the changed element can be invalidated.
    Siblings,
    /// Slotted elements can be invalidated.
    SlottedElements,
    /// Exposed parts can be invalidated.
    Parts,
    /// Relative selector anchors among ancestors can be invalidated.
    RelativeAncestors,
    /// A relative selector parent anchor can be invalidated.
    RelativeParent,
    /// A relative selector previous-sibling anchor can be invalidated.
    RelativePrevSibling,
    /// Relative selector ancestor previous-sibling anchors can be invalidated.
    RelativeAncestorPrevSibling,
    /// Relative selector earlier-sibling anchors can be invalidated.
    RelativeEarlierSibling,
    /// Relative selector ancestor earlier-sibling anchors can be invalidated.
    RelativeAncestorEarlierSibling,
    /// Scope dependencies can be invalidated.
    Scope,
}

/// Lightmount-facing action represented by one raw Stylo dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountDependencyInvalidationAction {
    /// The changed element itself can be invalidated.
    Element,
    /// The changed element and descendants can be invalidated.
    ElementAndDescendants,
    /// Descendants of the changed element can be invalidated.
    Descendants,
    /// Following siblings of the changed element can be invalidated.
    Siblings,
    /// Assigned/slotted descendants can be invalidated.
    SlottedElements,
    /// Exposed part descendants can be invalidated.
    Parts,
    /// Scope dependency handling needs the scope-specific retained path.
    Scope(LightmountScopeDependencyInvalidationAction),
    /// This dependency cannot be executed by Lightmount's retained path.
    Fallback(LightmountSourceInvalidationFallbackReason),
}

/// Sink for applying one retained dependency invalidation action.
pub trait LightmountDependencyInvalidationActionSink {
    /// The changed element itself should be invalidated.
    fn invalidate_element(&mut self);

    /// The changed element and its descendants should be invalidated.
    fn invalidate_element_and_descendants(&mut self);

    /// Descendants of the changed element should be invalidated.
    fn invalidate_descendants(&mut self);

    /// Following siblings of the changed element should be invalidated.
    fn invalidate_siblings(&mut self);

    /// Assigned/slotted descendants should be invalidated.
    fn invalidate_slotted_elements(&mut self);

    /// Exposed part descendants should be invalidated.
    fn invalidate_parts(&mut self);

    /// The dependency requires fallback handling.
    fn invalidate_fallback(&mut self, reason: LightmountSourceInvalidationFallbackReason);

    /// Scope-specific retained invalidation handling should run.
    fn invalidate_scope(&mut self, action: LightmountScopeDependencyInvalidationAction);
}

/// Scope dependency branch Lightmount should execute for one raw Stylo
/// dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountScopeDependencyInvalidationAction {
    /// The dependency is an implicit `@scope` edge and only its next
    /// dependencies should be propagated to descendants.
    ImplicitScope,
    /// The scope dependency must add invalidations at the subject.
    ForceAtSubject {
        /// Whether the subject add was forced by `:scope` in negation.
        force_add: bool,
    },
    /// Check next dependencies against the current element under this scope.
    CheckNextInScope,
    /// Push the scope dependency itself by the combinator to the right.
    PushByCombinator,
}

/// Sink for applying one scope dependency invalidation action.
pub trait LightmountScopeDependencyInvalidationActionSink {
    /// Propagate implicit `@scope` next dependencies to descendants.
    fn invalidate_implicit_scope(&mut self);

    /// Add invalidations at the scope subject.
    fn invalidate_scope_force_at_subject(&mut self, force_add: bool);

    /// Check next dependencies against the current element under this scope.
    fn invalidate_scope_check_next(&mut self);

    /// Push the scope dependency by the combinator to the right.
    fn invalidate_scope_by_combinator(&mut self);
}

/// Whether Lightmount's retained invalidation processor can execute one raw
/// dependency, and how its query result should be classified.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountRetainedProcessorDependencyEffect {
    /// The retained processor can execute this dependency.
    Retained {
        /// Whether an empty result for this dependency is an exact no-op.
        empty_result_is_exact: bool,
    },
    /// The dependency requires source-level fallback handling.
    Fallback(LightmountSourceInvalidationFallbackReason),
}

/// Relative selector candidate traversal represented by one raw Stylo
/// dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountRelativeDependencyInvalidationAction {
    /// Visit ancestor candidates.
    Ancestors,
    /// Visit the parent candidate.
    Parent,
    /// Visit the previous sibling candidate.
    PrevSibling,
    /// Visit previous siblings.
    EarlierSibling,
    /// Visit previous siblings of ancestors.
    AncestorPrevSibling,
    /// Visit earlier siblings of ancestors.
    AncestorEarlierSibling,
}

/// Sink for applying one relative selector candidate traversal action.
pub trait LightmountRelativeDependencyInvalidationActionSink {
    /// Visit ancestor candidates.
    fn visit_relative_ancestor_candidates(&mut self);

    /// Visit the parent candidate.
    fn visit_relative_parent_candidate(&mut self);

    /// Visit the previous sibling candidate.
    fn visit_relative_previous_sibling_candidate(&mut self);

    /// Visit previous sibling candidates.
    fn visit_relative_earlier_sibling_candidates(&mut self);

    /// Visit previous sibling candidates for ancestors.
    fn visit_relative_ancestor_previous_sibling_candidates(&mut self);

    /// Visit earlier sibling candidates for ancestors.
    fn visit_relative_ancestor_earlier_sibling_candidates(&mut self);
}

/// A Lightmount style invalidation query that can be answered from Stylo
/// invalidation maps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightmountStyleInvalidationQuery<'a> {
    /// An element matching `*` was inserted or removed.
    Universal,
    /// An element with this local name was inserted or removed.
    Type(&'a str),
    /// An attribute local-name changed on the root element.
    Attribute(&'a str),
    /// A class token was added or removed on the root element.
    Class(&'a str),
    /// An id value was added or removed on the root element.
    Id(&'a str),
    /// A pseudo-class state changed on the root element.
    State(ElementState),
    /// A custom state changed on the root element.
    CustomState(&'a str),
}

/// One pseudo-class state invalidation root derived for Lightmount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountStateInvalidationRoot<Root> {
    root: Root,
    state: ElementState,
}

/// Source-local retained invalidation query after the runtime-owned retained
/// query has been borrowed into Stylo invalidation-map query shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationQuery<'a, Root> {
    root: Root,
    query: LightmountStyleInvalidationQuery<'a>,
    previous_sibling: Option<Root>,
    next_sibling: Option<Root>,
}

impl<Root: Copy> LightmountStateInvalidationRoot<Root> {
    /// Create one state invalidation root.
    #[inline]
    pub fn new(root: Root, state: ElementState) -> Self {
        Self { root, state }
    }

    /// Return the root that should be invalidated for this state change.
    #[inline]
    pub fn root(&self) -> Root {
        self.root
    }

    /// Return the pseudo-class state represented by this root.
    #[inline]
    pub fn state(&self) -> ElementState {
        self.state
    }
}

/// Sibling context used when querying retained style invalidation for a root
/// that has already moved out of its original child list position.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LightmountRetainedStyleSiblingTraversal<Root> {
    previous_sibling: Option<Root>,
    next_sibling: Option<Root>,
}

impl<Root: Copy> LightmountRetainedStyleSiblingTraversal<Root> {
    /// Create sibling traversal context.
    #[inline]
    pub fn new(previous_sibling: Option<Root>, next_sibling: Option<Root>) -> Self {
        Self {
            previous_sibling,
            next_sibling,
        }
    }

    /// Return the previous sibling from the original child-list position.
    #[inline]
    pub fn previous_sibling(&self) -> Option<Root> {
        self.previous_sibling
    }

    /// Return the next sibling from the original child-list position.
    #[inline]
    pub fn next_sibling(&self) -> Option<Root> {
        self.next_sibling
    }
}

impl<'a, Root: Copy> LightmountSourceStyleInvalidationQuery<'a, Root> {
    /// Create one source-local invalidation query row.
    #[inline]
    pub fn new(
        root: Root,
        query: LightmountStyleInvalidationQuery<'a>,
        previous_sibling: Option<Root>,
        next_sibling: Option<Root>,
    ) -> Self {
        Self {
            root,
            query,
            previous_sibling,
            next_sibling,
        }
    }

    /// Return the query root.
    #[inline]
    pub fn root(&self) -> Root {
        self.root
    }

    /// Return the Stylo invalidation-map query shape.
    #[inline]
    pub fn query(&self) -> LightmountStyleInvalidationQuery<'a> {
        self.query
    }

    /// Return the mutation-time previous sibling, when available.
    #[inline]
    pub fn previous_sibling(&self) -> Option<Root> {
        self.previous_sibling
    }

    /// Return the mutation-time next sibling, when available.
    #[inline]
    pub fn next_sibling(&self) -> Option<Root> {
        self.next_sibling
    }
}

/// Owned retained invalidation query used by Lightmount's runtime queue.
///
/// The runtime owns mutation collection and cache clearing, but this keeps the
/// retained Stylo dependency query shape in the fork.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LightmountRetainedStyleInvalidationQuery<Root> {
    root: Root,
    kind: LightmountRetainedStyleInvalidationQueryKind,
    sibling_traversal: Option<LightmountRetainedStyleSiblingTraversal<Root>>,
}

/// Requirement for running a retained query against a stylesheet source's
/// dependency summary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LightmountSourceDependencyRequestRequirement {
    requires_child_list_structural_dependency: bool,
    requires_relative_previous_sibling_dependency: bool,
}

/// Mutation-time relation for fallback roots when the changed element has
/// already been inserted or removed from its original sibling position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountDependencyInvalidationFallbackContext<Root> {
    parent: Option<Root>,
    previous_sibling: Option<Root>,
    next_sibling: Option<Root>,
}

/// Per-element mutation before-state captured by Lightmount before retained
/// invalidation is drained through Stylo.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LightmountStyleMutationElementSnapshot {
    attribute_changes: IndexMap<String, Option<String>>,
    old_state: Option<ElementState>,
    old_custom_states: Option<Vec<String>>,
}

/// Materialized old element state used by Stylo's invalidation selector
/// matching for one Lightmount element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountStyleInvalidationSnapshot<Root> {
    element: Root,
    state: Option<ElementState>,
    custom_states: Option<Vec<String>>,
    changed_attributes: Vec<String>,
    attributes: Vec<LightmountStyleInvalidationSnapshotAttribute>,
}

/// One materialized attribute in a [`LightmountStyleInvalidationSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountStyleInvalidationSnapshotAttribute {
    local_name: String,
    name: String,
    namespace: String,
    prefix: Option<String>,
    value: String,
}

/// One retained mutation attribute change borrowed from
/// [`LightmountStyleMutationElementSnapshot`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountStyleMutationAttributeChange<'a> {
    name: &'a str,
    old_value: Option<&'a str>,
}

/// Context-derived fallback roots for one dependency query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountDependencyInvalidationContextRoots<Root> {
    requires_source_fallback: bool,
    roots: Vec<Root>,
}

/// Opaque context-root plan derived from a dependency query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountDependencyContextRootPlan {
    flags: LightmountDependencyContextRootFlags,
    allow_direct_previous_following_sibling_fallback: bool,
}

/// Adapter used by the source dependency planner when mutation-context roots
/// require DOM traversal.
pub trait LightmountSourceDependencyInvalidationContextRootsProvider<Root> {
    /// Build conservative context roots from a Stylo-derived root plan.
    fn context_roots_for_source_dependency(
        &mut self,
        root: Root,
        plan: LightmountDependencyContextRootPlan,
        context: LightmountDependencyInvalidationFallbackContext<Root>,
    ) -> LightmountDependencyInvalidationContextRoots<Root>;
}

/// Source-local invalidation request for one retained query, including
/// mutation context and source dependency gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountSourceDependencyInvalidationRequest<'a, Root> {
    query: &'a LightmountRetainedStyleInvalidationQuery<Root>,
    context: Option<LightmountDependencyInvalidationFallbackContext<Root>>,
    requirement: LightmountSourceDependencyRequestRequirement,
}

/// Selector-derived keys for DOM boundaries whose child structure can affect
/// a source's tree-structural selectors.
///
/// These keys are separate from normal state/attribute invalidation metadata:
/// selectors such as `section:empty` and
/// `details > summary:first-of-type` are driven by mutations to the boundary
/// element even though no attribute on the selector subject changed.
#[derive(Clone, Debug, Default, Eq, Hash, MallocSizeOf, PartialEq)]
pub(crate) struct LightmountChildListStructuralBoundaryDependencySummary {
    class_dependencies: Vec<Atom>,
    id_dependencies: Vec<Atom>,
    type_dependencies: Vec<LocalName>,
    attribute_dependencies: Vec<LocalName>,
    universal_dependency: bool,
}

/// Source-local Stylo dependency metadata used by Lightmount invalidation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct LightmountSourceDependencySummary {
    dependency_summary: LightmountDependencyInvalidationSummary,
    has_child_list_structural_dependency: bool,
    child_list_structural_boundary_dependencies:
        LightmountChildListStructuralBoundaryDependencySummary,
}

/// One stylesheet source participating in a source dependency invalidation
/// batch.
pub struct LightmountSourceDependencyInvalidationBatchSource<'a, Root> {
    dependency_summary: &'a LightmountSourceDependencySummary,
    fallback_roots: LightmountSourceDependencyFallbackRoots<'a, Root>,
}

/// Fallback roots available for one stylesheet source dependency batch.
///
/// Source-local roots describe the stylesheet source's own scope. Cause roots
/// describe a narrower runtime mutation boundary when one is available. The
/// Stylo-facing source input owns the policy for choosing between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LightmountSourceDependencyFallbackRoots<'a, Root> {
    source_local_roots: &'a [Root],
    cause_roots: &'a [Root],
}

/// Retained invalidation queries and base cleanup roots for child-list changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountRetainedStyleChildListInvalidationQueries<Root> {
    queries: Vec<LightmountRetainedStyleChildListInvalidationQuery<Root>>,
    base_roots: Vec<Root>,
    empty_target_fallback_roots: Vec<Root>,
    relative_previous_sibling_cleanup_roots: Vec<Root>,
}

/// Sink used to drain child-list retained invalidation query batches into a
/// runtime-owned pending plan.
pub trait LightmountRetainedStyleChildListInvalidationQueriesSink<Root> {
    /// Record one retained query and its source dependency requirement.
    fn record_child_list_retained_query(
        &mut self,
        query: LightmountRetainedStyleInvalidationQuery<Root>,
        requirement: LightmountSourceDependencyRequestRequirement,
    );

    /// Extend base structural-boundary cleanup roots.
    fn extend_child_list_base_roots(&mut self, roots: Vec<Root>);

    /// Extend empty-target fallback roots.
    fn extend_child_list_empty_target_fallback_roots(&mut self, roots: Vec<Root>);

    /// Extend direct previous-sibling relative cleanup roots.
    fn extend_child_list_relative_previous_sibling_cleanup_roots(&mut self, roots: Vec<Root>);
}

/// Builder for retained invalidation queries and cleanup roots produced from
/// child-list mutation facts.
#[derive(Clone, Debug)]
pub struct LightmountRetainedStyleChildListInvalidationQueryBuilder<Root: Eq + Hash> {
    queries: IndexMap<
        LightmountRetainedStyleInvalidationQuery<Root>,
        LightmountSourceDependencyRequestRequirement,
    >,
    base_roots: IndexSet<Root>,
    empty_target_fallback_roots: IndexSet<Root>,
    relative_previous_sibling_cleanup_roots: IndexSet<Root>,
}

/// The child-list sibling boundary whose retained cleanup buckets are being
/// materialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightmountChildListSiblingBoundaryKind {
    /// The previous element sibling around an inserted range.
    AddedPreviousSibling {
        /// Whether the inserted range was appended at the end of the parent.
        inserted_at_end: bool,
    },
    /// The next element sibling around an inserted range.
    AddedNextSibling,
    /// The previous element sibling around a removed range.
    RemovedPreviousSibling,
    /// The next element sibling around a removed range.
    RemovedNextSibling,
    /// The sibling before the previous boundary around a removed range.
    RemovedEarlierSibling,
}

/// Retained cleanup bucket decisions for one child-list sibling boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountChildListSiblingBoundaryPlan<Root> {
    root: Root,
    include_base_root: bool,
    include_empty_target_fallback_root: bool,
    include_relative_previous_sibling_cleanup_root: bool,
}

/// One child-list retained invalidation query and whether it should only run
/// against sources with specific child-list dependency gates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountRetainedStyleChildListInvalidationQuery<Root> {
    query: LightmountRetainedStyleInvalidationQuery<Root>,
    requirement: LightmountSourceDependencyRequestRequirement,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountRetainedStyleInvalidationQueryKind {
    Universal,
    Type { local_name: String },
    Attribute { name: String },
    Class { token: String },
    Id { value: String },
    State { state: ElementState },
    CustomState { name: String },
}

impl Hash for LightmountRetainedStyleInvalidationQueryKind {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Universal => {
                0u8.hash(state);
            },
            Self::Type { local_name } => {
                1u8.hash(state);
                local_name.hash(state);
            },
            Self::Attribute { name } => {
                2u8.hash(state);
                name.hash(state);
            },
            Self::Class { token } => {
                3u8.hash(state);
                token.hash(state);
            },
            Self::Id { value } => {
                4u8.hash(state);
                value.hash(state);
            },
            Self::State {
                state: element_state,
            } => {
                5u8.hash(state);
                element_state.bits().hash(state);
            },
            Self::CustomState { name } => {
                6u8.hash(state);
                name.hash(state);
            },
        }
    }
}

impl<Root: Copy> LightmountRetainedStyleInvalidationQuery<Root> {
    /// Create a universal retained query.
    #[inline]
    pub fn universal(root: Root) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::Universal,
            sibling_traversal: None,
        }
    }

    /// Create a type retained query.
    #[inline]
    pub fn element_type(root: Root, local_name: String) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::Type { local_name },
            sibling_traversal: None,
        }
    }

    /// Create an attribute retained query.
    #[inline]
    pub fn attribute(root: Root, name: String) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::Attribute { name },
            sibling_traversal: None,
        }
    }

    /// Create a class retained query.
    #[inline]
    pub fn class(root: Root, token: String) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::Class { token },
            sibling_traversal: None,
        }
    }

    /// Create an id retained query.
    #[inline]
    pub fn id(root: Root, value: String) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::Id { value },
            sibling_traversal: None,
        }
    }

    /// Create a state retained query.
    #[inline]
    pub fn state(root: Root, state: ElementState) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::State { state },
            sibling_traversal: None,
        }
    }

    /// Create a custom-state retained query.
    #[inline]
    pub fn custom_state(root: Root, name: String) -> Self {
        Self {
            root,
            kind: LightmountRetainedStyleInvalidationQueryKind::CustomState { name },
            sibling_traversal: None,
        }
    }

    /// Attach sibling traversal context.
    #[inline]
    pub fn with_sibling_traversal(
        mut self,
        sibling_traversal: Option<LightmountRetainedStyleSiblingTraversal<Root>>,
    ) -> Self {
        self.sibling_traversal = sibling_traversal;
        self
    }

    /// Return the query root.
    #[inline]
    pub fn root(&self) -> Root {
        self.root
    }

    /// Return sibling traversal context.
    #[inline]
    pub fn sibling_traversal(&self) -> Option<LightmountRetainedStyleSiblingTraversal<Root>> {
        self.sibling_traversal
    }

    /// Return whether this query targets the universal invalidation map.
    #[inline]
    pub fn is_universal(&self) -> bool {
        matches!(
            self.kind,
            LightmountRetainedStyleInvalidationQueryKind::Universal
        )
    }

    /// Return whether this state query can use direct previous-sibling
    /// fallback roots when child-list sibling context is available.
    #[inline]
    pub fn allows_direct_previous_following_sibling_fallback(&self) -> bool {
        matches!(
            self.kind,
            LightmountRetainedStyleInvalidationQueryKind::State { state }
                if state.intersects(ElementState::HEADING_LEVEL_BITS)
        )
    }

    /// Borrow this retained query as the Stylo invalidation-map query shape.
    #[inline]
    pub fn as_stylo_query(&self) -> LightmountStyleInvalidationQuery<'_> {
        match &self.kind {
            LightmountRetainedStyleInvalidationQueryKind::Universal => {
                LightmountStyleInvalidationQuery::Universal
            },
            LightmountRetainedStyleInvalidationQueryKind::Type { local_name } => {
                LightmountStyleInvalidationQuery::Type(local_name)
            },
            LightmountRetainedStyleInvalidationQueryKind::Attribute { name } => {
                LightmountStyleInvalidationQuery::Attribute(name)
            },
            LightmountRetainedStyleInvalidationQueryKind::Class { token } => {
                LightmountStyleInvalidationQuery::Class(token)
            },
            LightmountRetainedStyleInvalidationQueryKind::Id { value } => {
                LightmountStyleInvalidationQuery::Id(value)
            },
            LightmountRetainedStyleInvalidationQueryKind::State { state } => {
                LightmountStyleInvalidationQuery::State(*state)
            },
            LightmountRetainedStyleInvalidationQueryKind::CustomState { name } => {
                LightmountStyleInvalidationQuery::CustomState(name)
            },
        }
    }

    /// Borrow this retained query as a source-local invalidation query row.
    #[inline]
    pub fn as_source_query(&self) -> LightmountSourceStyleInvalidationQuery<'_, Root> {
        let sibling_traversal = self.sibling_traversal();
        LightmountSourceStyleInvalidationQuery::new(
            self.root(),
            self.as_stylo_query(),
            sibling_traversal.and_then(|traversal| traversal.previous_sibling()),
            sibling_traversal.and_then(|traversal| traversal.next_sibling()),
        )
    }
}

/// Dependency keys captured for one element before retained invalidation query
/// construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountElementDependencySnapshot<Root> {
    handle: Root,
    local_name: String,
    state: ElementState,
    attribute_names: Vec<String>,
    class_tokens: Vec<String>,
    custom_states: Vec<String>,
    id: Option<String>,
}

impl<Root: Hash> Hash for LightmountElementDependencySnapshot<Root> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.handle.hash(state);
        self.local_name.hash(state);
        self.state.bits().hash(state);
        self.attribute_names.hash(state);
        self.class_tokens.hash(state);
        self.custom_states.hash(state);
        self.id.hash(state);
    }
}

impl<Root: Copy> LightmountElementDependencySnapshot<Root> {
    /// Create dependency snapshot keys for one element.
    #[inline]
    pub fn new(
        handle: Root,
        local_name: String,
        state: ElementState,
        attribute_names: Vec<String>,
        class_tokens: Vec<String>,
        custom_states: Vec<String>,
        id: Option<String>,
    ) -> Self {
        Self {
            handle,
            local_name,
            state,
            attribute_names,
            class_tokens,
            custom_states,
            id,
        }
    }

    /// Return the element handle this snapshot describes.
    #[inline]
    pub fn handle(&self) -> Root {
        self.handle
    }

    /// Return captured class tokens.
    #[inline]
    pub fn class_tokens(&self) -> &[String] {
        &self.class_tokens
    }
}

/// Borrowed child-list mutation facts used to derive retained dependency
/// fallback context for a retained query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountRetainedStyleChildListMutationContext<'a, Root> {
    parent: Root,
    added_nodes: &'a [Root],
    removed_nodes: &'a [Root],
    removed_element_snapshots: &'a [LightmountElementDependencySnapshot<Root>],
    previous_sibling: Option<Root>,
    next_sibling: Option<Root>,
}

impl<'a, Root> LightmountRetainedStyleChildListMutationContext<'a, Root>
where
    Root: Copy + Eq + 'a,
{
    /// Create child-list mutation context from runtime-captured mutation facts.
    #[inline]
    pub fn new(
        parent: Root,
        added_nodes: &'a [Root],
        removed_nodes: &'a [Root],
        removed_element_snapshots: &'a [LightmountElementDependencySnapshot<Root>],
        previous_sibling: Option<Root>,
        next_sibling: Option<Root>,
    ) -> Self {
        Self {
            parent,
            added_nodes,
            removed_nodes,
            removed_element_snapshots,
            previous_sibling,
            next_sibling,
        }
    }

    fn contains_query_root(&self, root: Root) -> bool {
        self.parent == root
            || self.added_nodes.contains(&root)
            || self.removed_nodes.contains(&root)
            || self.previous_sibling == Some(root)
            || self.next_sibling == Some(root)
            || self
                .removed_element_snapshots
                .iter()
                .any(|snapshot| snapshot.handle() == root)
    }

    /// Return the retained dependency fallback context for this query when it
    /// belongs to this child-list mutation context.
    pub fn fallback_context_for_query(
        &self,
        query: &LightmountRetainedStyleInvalidationQuery<Root>,
    ) -> Option<LightmountDependencyInvalidationFallbackContext<Root>> {
        if !self.contains_query_root(query.root()) {
            return None;
        }
        let traversal = query.sibling_traversal();
        Some(
            LightmountDependencyInvalidationFallbackContext::from_mutation_relation(
                Some(self.parent),
                traversal
                    .and_then(|traversal| traversal.previous_sibling())
                    .or(self.previous_sibling),
                traversal
                    .and_then(|traversal| traversal.next_sibling())
                    .or(self.next_sibling),
            ),
        )
    }
}

/// Return the first child-list mutation fallback context matching a retained
/// query.
pub fn lightmount_child_list_dependency_fallback_context_for_query<'a, Root>(
    contexts: impl IntoIterator<Item = LightmountRetainedStyleChildListMutationContext<'a, Root>>,
    query: &LightmountRetainedStyleInvalidationQuery<Root>,
) -> Option<LightmountDependencyInvalidationFallbackContext<Root>>
where
    Root: Copy + Eq + 'a,
{
    contexts
        .into_iter()
        .find_map(|context| context.fallback_context_for_query(query))
}

/// Build retained dependency queries from captured element dependency keys.
pub fn lightmount_retained_queries_for_element_dependency_snapshot<Root: Copy>(
    snapshot: &LightmountElementDependencySnapshot<Root>,
    sibling_traversal: Option<LightmountRetainedStyleSiblingTraversal<Root>>,
) -> Vec<LightmountRetainedStyleInvalidationQuery<Root>> {
    lightmount_retained_queries_for_element_dependency_snapshot_with_universal(
        snapshot,
        sibling_traversal,
        true,
    )
}

/// Build non-universal retained dependency queries from captured element keys.
pub fn lightmount_retained_non_universal_queries_for_element_dependency_snapshot<Root: Copy>(
    snapshot: &LightmountElementDependencySnapshot<Root>,
    sibling_traversal: Option<LightmountRetainedStyleSiblingTraversal<Root>>,
) -> Vec<LightmountRetainedStyleInvalidationQuery<Root>> {
    lightmount_retained_queries_for_element_dependency_snapshot_with_universal(
        snapshot,
        sibling_traversal,
        false,
    )
}

fn lightmount_retained_queries_for_element_dependency_snapshot_with_universal<Root: Copy>(
    snapshot: &LightmountElementDependencySnapshot<Root>,
    sibling_traversal: Option<LightmountRetainedStyleSiblingTraversal<Root>>,
    include_universal: bool,
) -> Vec<LightmountRetainedStyleInvalidationQuery<Root>> {
    let mut queries = Vec::new();
    if include_universal {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::universal(snapshot.handle)
                .with_sibling_traversal(sibling_traversal),
        );
    }
    queries.push(
        LightmountRetainedStyleInvalidationQuery::element_type(
            snapshot.handle,
            snapshot.local_name.clone(),
        )
        .with_sibling_traversal(sibling_traversal),
    );
    if !snapshot.state.is_empty() {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::state(snapshot.handle, snapshot.state)
                .with_sibling_traversal(sibling_traversal),
        );
    }
    for attribute_name in &snapshot.attribute_names {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::attribute(
                snapshot.handle,
                attribute_name.to_owned(),
            )
            .with_sibling_traversal(sibling_traversal),
        );
    }
    for token in &snapshot.class_tokens {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::class(snapshot.handle, token.to_owned())
                .with_sibling_traversal(sibling_traversal),
        );
    }
    if let Some(id) = &snapshot.id {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::id(snapshot.handle, id.to_owned())
                .with_sibling_traversal(sibling_traversal),
        );
    }
    for state in &snapshot.custom_states {
        queries.push(
            LightmountRetainedStyleInvalidationQuery::custom_state(snapshot.handle, state.clone())
                .with_sibling_traversal(sibling_traversal),
        );
    }
    queries
}

impl LightmountSourceDependencyRequestRequirement {
    /// No additional source dependency gate is required.
    #[inline]
    pub fn exact() -> Self {
        Self::default()
    }

    /// The source must contain child-list structural dependencies.
    #[inline]
    pub fn child_list_structural() -> Self {
        Self {
            requires_child_list_structural_dependency: true,
            requires_relative_previous_sibling_dependency: false,
        }
    }

    /// The source must contain direct previous-sibling relative dependencies.
    #[inline]
    pub fn relative_previous_sibling() -> Self {
        Self {
            requires_child_list_structural_dependency: false,
            requires_relative_previous_sibling_dependency: true,
        }
    }

    /// The source must contain both child-list structural and direct
    /// previous-sibling relative dependencies.
    #[inline]
    pub fn child_list_structural_relative_previous_sibling() -> Self {
        Self {
            requires_child_list_structural_dependency: true,
            requires_relative_previous_sibling_dependency: true,
        }
    }

    /// Merge requirements for duplicate retained queries.
    #[inline]
    fn merged_with(self, incoming: Self) -> Self {
        Self {
            requires_child_list_structural_dependency: self
                .requires_child_list_structural_dependency
                && incoming.requires_child_list_structural_dependency,
            requires_relative_previous_sibling_dependency: self
                .requires_relative_previous_sibling_dependency
                || incoming.requires_relative_previous_sibling_dependency,
        }
    }

    /// Return whether child-list structural dependencies are required.
    #[inline]
    pub fn requires_child_list_structural_dependency(self) -> bool {
        self.requires_child_list_structural_dependency
    }

    /// Return whether direct previous-sibling relative dependencies are
    /// required.
    #[inline]
    pub fn requires_relative_previous_sibling_dependency(self) -> bool {
        self.requires_relative_previous_sibling_dependency
    }
}

/// Merge source dependency request requirements for duplicate retained queries.
#[inline]
pub fn lightmount_merge_source_dependency_request_requirement(
    existing: LightmountSourceDependencyRequestRequirement,
    incoming: LightmountSourceDependencyRequestRequirement,
) -> LightmountSourceDependencyRequestRequirement {
    existing.merged_with(incoming)
}

impl<Root> LightmountDependencyInvalidationFallbackContext<Root> {
    /// Create fallback context from the mutation-time sibling relation.
    #[inline]
    pub fn from_mutation_relation(
        parent: Option<Root>,
        previous_sibling: Option<Root>,
        next_sibling: Option<Root>,
    ) -> Self {
        Self {
            parent,
            previous_sibling,
            next_sibling,
        }
    }

    /// Return the mutation-time parent.
    #[inline]
    pub fn parent(&self) -> Option<Root>
    where
        Root: Copy,
    {
        self.parent
    }

    /// Return the mutation-time previous sibling.
    #[inline]
    pub fn previous_sibling(&self) -> Option<Root>
    where
        Root: Copy,
    {
        self.previous_sibling
    }

    /// Return the mutation-time next sibling.
    #[inline]
    pub fn next_sibling(&self) -> Option<Root>
    where
        Root: Copy,
    {
        self.next_sibling
    }
}

impl<Root> Default for LightmountDependencyInvalidationFallbackContext<Root> {
    #[inline]
    fn default() -> Self {
        Self {
            parent: None,
            previous_sibling: None,
            next_sibling: None,
        }
    }
}

impl LightmountStyleMutationElementSnapshot {
    /// Record one attribute's old value, keeping the first old value observed
    /// in a mutation batch.
    #[inline]
    pub fn record_attribute_change(&mut self, name: &str, old_value: Option<String>) {
        self.attribute_changes
            .entry(name.to_owned())
            .or_insert(old_value);
    }

    /// Record the element's old state if no old state has already been
    /// captured for this element.
    #[inline]
    pub fn try_record_old_state(&mut self, old_state: ElementState) -> Option<()> {
        if self.old_state.is_some() {
            return None;
        }
        self.old_state = Some(old_state);
        Some(())
    }

    /// Record the old custom states if this element has not already captured
    /// them in the current mutation batch.
    #[inline]
    pub fn record_old_custom_states(&mut self, old_custom_states: Vec<String>) {
        self.old_custom_states.get_or_insert(old_custom_states);
    }

    /// Merge another element snapshot into this one, preserving first observed
    /// old values and states.
    pub fn merge_from(&mut self, incoming: Self) {
        for (name, old_value) in incoming.attribute_changes {
            self.record_attribute_change(&name, old_value);
        }
        if self.old_state.is_none() {
            self.old_state = incoming.old_state;
        }
        if self.old_custom_states.is_none() {
            self.old_custom_states = incoming.old_custom_states;
        }
    }

    /// Return captured attribute changes in insertion order.
    pub fn attribute_changes(
        &self,
    ) -> impl Iterator<Item = LightmountStyleMutationAttributeChange<'_>> {
        self.attribute_changes.iter().map(|(name, old_value)| {
            LightmountStyleMutationAttributeChange {
                name,
                old_value: old_value.as_deref(),
            }
        })
    }

    /// Return the number of captured attribute changes.
    #[inline]
    pub fn attribute_change_count(&self) -> usize {
        self.attribute_changes.len()
    }

    /// Return the captured old element state.
    #[inline]
    pub fn old_state(&self) -> Option<ElementState> {
        self.old_state
    }

    /// Return the captured old custom states.
    #[inline]
    pub fn old_custom_states(&self) -> Option<&[String]> {
        self.old_custom_states.as_deref()
    }
}

impl<Root: Copy> LightmountStyleInvalidationSnapshot<Root> {
    /// Create a materialized invalidation snapshot.
    #[inline]
    pub fn new(
        element: Root,
        state: Option<ElementState>,
        custom_states: Option<Vec<String>>,
        changed_attributes: Vec<String>,
        attributes: Vec<LightmountStyleInvalidationSnapshotAttribute>,
    ) -> Self {
        Self {
            element,
            state,
            custom_states,
            changed_attributes,
            attributes,
        }
    }

    /// Return the element this snapshot belongs to.
    #[inline]
    pub fn element(&self) -> Root {
        self.element
    }

    /// Return the old pseudo-class state, when captured.
    #[inline]
    pub fn state(&self) -> Option<ElementState> {
        self.state
    }

    /// Return the old custom-state set, when captured.
    #[inline]
    pub fn custom_states(&self) -> Option<&[String]> {
        self.custom_states.as_deref()
    }

    /// Return old attribute values after applying captured mutation facts.
    #[inline]
    pub fn attributes(&self) -> &[LightmountStyleInvalidationSnapshotAttribute] {
        &self.attributes
    }

    /// Return attribute local names changed by this mutation.
    #[inline]
    pub fn changed_attributes(&self) -> &[String] {
        &self.changed_attributes
    }
}

impl LightmountStyleInvalidationSnapshotAttribute {
    /// Create one materialized invalidation snapshot attribute.
    #[inline]
    pub fn new(
        local_name: String,
        name: String,
        namespace: String,
        prefix: Option<String>,
        value: String,
    ) -> Self {
        Self {
            local_name,
            name,
            namespace,
            prefix,
            value,
        }
    }

    /// Return the local name.
    #[inline]
    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    /// Return the qualified attribute name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the namespace string.
    #[inline]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the prefix, when present.
    #[inline]
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// Return the attribute value.
    #[inline]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replace the materialized attribute value.
    #[inline]
    pub fn set_value(&mut self, value: String) {
        self.value = value;
    }

    /// Return whether this attribute has no namespace and the given local name.
    #[inline]
    pub fn is_no_namespace_local_name(&self, local_name: &str) -> bool {
        self.namespace.is_empty() && self.local_name == local_name
    }
}

impl<'a> LightmountStyleMutationAttributeChange<'a> {
    /// Return the changed attribute local name.
    #[inline]
    pub fn name(&self) -> &str {
        self.name
    }

    /// Return the captured old attribute value.
    #[inline]
    pub fn old_value(&self) -> Option<&str> {
        self.old_value
    }
}

impl<Root> LightmountDependencyInvalidationContextRoots<Root> {
    /// Create context-derived fallback roots.
    #[inline]
    fn new(requires_source_fallback: bool, roots: Vec<Root>) -> Self {
        Self {
            requires_source_fallback,
            roots,
        }
    }

    /// Returns whether these roots are insufficient for exact-safety fallback
    /// and source fallback roots are required.
    #[inline]
    fn requires_source_fallback(&self) -> bool {
        self.requires_source_fallback
    }

    /// Return the context-derived roots.
    #[inline]
    fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Consume this value and return the context-derived roots.
    #[inline]
    fn into_roots(self) -> Vec<Root> {
        self.roots
    }
}

impl<'a, Root> LightmountSourceDependencyInvalidationRequest<'a, Root>
where
    Root: Copy,
{
    /// Create a source dependency invalidation request.
    #[inline]
    pub fn new(
        query: &'a LightmountRetainedStyleInvalidationQuery<Root>,
        context: Option<LightmountDependencyInvalidationFallbackContext<Root>>,
        requirement: LightmountSourceDependencyRequestRequirement,
    ) -> Self {
        Self {
            query,
            context,
            requirement,
        }
    }

    /// Return the retained query for this request.
    #[inline]
    fn query(&self) -> &'a LightmountRetainedStyleInvalidationQuery<Root> {
        self.query
    }

    /// Return the optional mutation-time fallback context.
    #[inline]
    fn context(&self) -> Option<LightmountDependencyInvalidationFallbackContext<Root>> {
        self.context
    }

    /// Return whether this request requires child-list structural dependencies
    /// in the source.
    #[inline]
    fn requires_child_list_structural_dependency(&self) -> bool {
        self.requirement.requires_child_list_structural_dependency()
    }

    /// Return whether this request requires direct previous-sibling relative
    /// dependencies in the source.
    #[inline]
    fn requires_relative_previous_sibling_dependency(&self) -> bool {
        self.requirement
            .requires_relative_previous_sibling_dependency()
    }
}

impl LightmountChildListStructuralBoundaryDependencySummary {
    #[inline]
    pub(crate) fn note_class_dependency(&mut self, class: Atom) {
        if !self.class_dependencies.contains(&class) {
            self.class_dependencies.push(class);
        }
    }

    #[inline]
    pub(crate) fn note_id_dependency(&mut self, id: Atom) {
        if !self.id_dependencies.contains(&id) {
            self.id_dependencies.push(id);
        }
    }

    #[inline]
    pub(crate) fn note_type_dependency(&mut self, local_name: LocalName) {
        if !self.type_dependencies.contains(&local_name) {
            self.type_dependencies.push(local_name);
        }
    }

    #[inline]
    pub(crate) fn note_attribute_dependency(&mut self, attribute: LocalName) {
        if !self.attribute_dependencies.contains(&attribute) {
            self.attribute_dependencies.push(attribute);
        }
    }

    #[inline]
    pub(crate) fn note_universal_dependency(&mut self) {
        self.universal_dependency = true;
    }

    #[inline]
    pub(crate) fn matches_query(&self, query: LightmountStyleInvalidationQuery<'_>) -> bool {
        match query {
            LightmountStyleInvalidationQuery::Universal => self.universal_dependency,
            LightmountStyleInvalidationQuery::Type(local_name) => self
                .type_dependencies
                .contains(&LocalName::from(local_name)),
            LightmountStyleInvalidationQuery::Attribute(name) => {
                self.attribute_dependencies.contains(&LocalName::from(name))
            },
            LightmountStyleInvalidationQuery::Class(token) => {
                self.class_dependencies.contains(&Atom::from(token))
            },
            LightmountStyleInvalidationQuery::Id(value) => {
                self.id_dependencies.contains(&Atom::from(value))
            },
            LightmountStyleInvalidationQuery::State(_)
            | LightmountStyleInvalidationQuery::CustomState(_) => false,
        }
    }
}

impl LightmountSourceDependencySummary {
    /// Create source dependency metadata from raw Stylo-derived summary parts.
    #[inline]
    pub(crate) fn new(
        dependency_summary: LightmountDependencyInvalidationSummary,
        has_child_list_structural_dependency: bool,
        child_list_structural_boundary_dependencies:
            LightmountChildListStructuralBoundaryDependencySummary,
    ) -> Self {
        Self {
            dependency_summary,
            has_child_list_structural_dependency,
            child_list_structural_boundary_dependencies,
        }
    }

    /// Create source dependency metadata from Stylo cascade data.
    #[inline]
    pub fn from_cascade_data(cascade_data: &CascadeData) -> Self {
        Self::new(
            cascade_data.lightmount_dependency_invalidation_summary(),
            cascade_data.has_child_list_structural_dependency(),
            cascade_data
                .lightmount_child_list_structural_boundary_dependency_summary()
                .clone(),
        )
    }

    /// Query dependencies for a changed class through this source summary.
    #[inline]
    pub fn query_class(&self, class: &Atom) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_class(class)
    }

    /// Query dependencies for a changed id through this source summary.
    #[inline]
    pub fn query_id(&self, id: &Atom) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_id(id)
    }

    /// Query dependencies for a changed attribute through this source summary.
    #[inline]
    pub fn query_attribute(&self, attribute: &LocalName) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_attribute(attribute)
    }

    /// Query dependencies for an inserted or removed element local name.
    #[inline]
    pub fn query_type(&self, local_name: &LocalName) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_type(local_name)
    }

    /// Query dependencies for an inserted or removed element matching `*`.
    #[inline]
    pub fn query_universal(&self) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_universal()
    }

    /// Query dependencies for a changed element state bitset.
    #[inline]
    pub fn query_state(&self, state: ElementState) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_state(state)
    }

    /// Query dependencies for a changed CSS custom state.
    #[inline]
    pub fn query_custom_state(&self, state: &AtomIdent) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_custom_state(state)
    }

    /// Query dependencies for focus-like state changes.
    #[inline]
    pub fn query_focus(&self) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_focus()
    }

    /// Query dependencies for `:focus-within` state changes.
    #[inline]
    pub fn query_focus_within(&self) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_focus_within()
    }

    /// Query dependencies for `:target` state changes.
    #[inline]
    pub fn query_target(&self) -> LightmountDependencyQueryResult {
        self.dependency_summary.query_target()
    }

    /// Return whether this source has child-list structural dependencies.
    #[inline]
    pub fn has_child_list_structural_dependency(&self) -> bool {
        self.has_child_list_structural_dependency
    }

    #[inline]
    fn has_child_list_structural_boundary_dependency_for_request<Root>(
        &self,
        request: &LightmountSourceDependencyInvalidationRequest<'_, Root>,
    ) -> bool
    where
        Root: Copy,
    {
        self.has_child_list_structural_dependency
            && request.requires_child_list_structural_dependency()
            && self
                .child_list_structural_boundary_dependencies
                .matches_query(request.query().as_stylo_query())
    }

    /// Return whether this source has any relative selector dependency.
    #[inline]
    pub fn has_relative_selector_dependency(&self) -> bool {
        self.dependency_summary.has_relative_selector_dependency()
    }

    /// Return whether this source has any focus-like state dependency.
    #[inline]
    pub fn has_focus_dependency(&self) -> bool {
        self.dependency_summary.query_focus().has_any_dependency()
    }

    /// Return whether this source has any `:focus-within` dependency.
    #[inline]
    pub fn has_focus_within_dependency(&self) -> bool {
        self.dependency_summary
            .query_focus_within()
            .has_any_dependency()
    }

    /// Return whether this source has any `:target` dependency.
    #[inline]
    pub fn has_target_dependency(&self) -> bool {
        self.dependency_summary.query_target().has_any_dependency()
    }

    /// Return whether this source has any following-sibling dependency.
    #[inline]
    pub fn has_sibling_dependency(&self) -> bool {
        self.dependency_summary.has_sibling_dependency()
    }

    /// Query this source dependency summary for one retained invalidation
    /// query shape.
    #[inline]
    pub fn query_result(
        &self,
        query: LightmountStyleInvalidationQuery<'_>,
    ) -> LightmountDependencyQueryResult {
        match query {
            LightmountStyleInvalidationQuery::Universal => self.query_universal(),
            LightmountStyleInvalidationQuery::Type(local_name) => {
                self.query_type(&LocalName::from(local_name))
            },
            LightmountStyleInvalidationQuery::Attribute(name) => {
                self.query_attribute(&LocalName::from(name))
            },
            LightmountStyleInvalidationQuery::Class(token) => self.query_class(&Atom::from(token)),
            LightmountStyleInvalidationQuery::Id(value) => self.query_id(&Atom::from(value)),
            LightmountStyleInvalidationQuery::State(state) => self.query_state(state),
            LightmountStyleInvalidationQuery::CustomState(name) => {
                self.query_custom_state(&AtomIdent::from(name))
            },
        }
    }

    /// Return whether child-list structural dependencies can participate in
    /// any request that requires them.
    ///
    /// The source-level structural bit is intentionally paired with
    /// selector-derived boundary keys. A structural selector elsewhere in the
    /// same source must not turn an unrelated type or universal query into an
    /// empty-target fallback.
    #[inline]
    fn has_child_list_structural_dependency_for_requests<Root>(
        &self,
        requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    ) -> bool
    where
        Root: Copy,
    {
        requests
            .iter()
            .any(|request| self.has_child_list_structural_boundary_dependency_for_request(request))
    }

    /// Return whether direct previous-sibling relative dependencies are
    /// present for any request.
    #[inline]
    fn has_relative_previous_sibling_dependency_for_requests<Root>(
        &self,
        requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    ) -> bool
    where
        Root: Copy,
    {
        requests.iter().any(|request| {
            self.query_result(request.query().as_stylo_query())
                .has_relative_previous_sibling_dependency()
        })
    }

    /// Return whether slotted dependencies are present for any request.
    #[inline]
    fn has_slotted_dependency_for_requests<Root>(
        &self,
        requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    ) -> bool
    where
        Root: Copy,
    {
        requests.iter().any(|request| {
            self.query_result(request.query().as_stylo_query())
                .has_slotted_dependency()
        })
    }

    /// Return whether this source needs an empty-target fallback for the
    /// requested source dependency batch.
    #[inline]
    fn requires_empty_target_fallback_for_requests<Root>(
        &self,
        requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    ) -> bool
    where
        Root: Copy,
    {
        self.has_child_list_structural_dependency_for_requests(requests)
            || self.has_relative_previous_sibling_dependency_for_requests(requests)
            || self.has_slotted_dependency_for_requests(requests)
    }

    /// Return structural-boundary cleanup roots for the requested source
    /// dependency batch.
    #[inline]
    fn structural_boundary_cleanup_roots_for_requests<Root>(
        &self,
        requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
        relative_previous_sibling_cleanup_roots: &[Root],
    ) -> Vec<Root>
    where
        Root: Copy,
    {
        if self.has_relative_previous_sibling_dependency_for_requests(requests) {
            relative_previous_sibling_cleanup_roots.to_vec()
        } else {
            Vec::new()
        }
    }
}

impl<'a, Root> LightmountSourceDependencyInvalidationBatchSource<'a, Root> {
    /// Create one source dependency invalidation batch source.
    #[inline]
    pub fn new(
        dependency_summary: &'a LightmountSourceDependencySummary,
        source_local_fallback_roots: &'a [Root],
        cause_fallback_roots: &'a [Root],
    ) -> Self {
        Self {
            dependency_summary,
            fallback_roots: LightmountSourceDependencyFallbackRoots::new(
                source_local_fallback_roots,
                cause_fallback_roots,
            ),
        }
    }

    /// Return this source's dependency summary.
    #[inline]
    fn dependency_summary(&self) -> &'a LightmountSourceDependencySummary {
        self.dependency_summary
    }

    /// Return the selected fallback roots, preferring cause roots when present.
    #[inline]
    fn selected_fallback_roots(&self) -> &'a [Root] {
        self.fallback_roots.selected_roots()
    }
}

impl<'a, Root> LightmountSourceDependencyFallbackRoots<'a, Root> {
    fn new(source_local_roots: &'a [Root], cause_roots: &'a [Root]) -> Self {
        Self {
            source_local_roots,
            cause_roots,
        }
    }

    fn selected_roots(&self) -> &'a [Root] {
        if self.cause_roots.is_empty() {
            self.source_local_roots
        } else {
            self.cause_roots
        }
    }
}

impl<Root> LightmountRetainedStyleChildListInvalidationQueries<Root> {
    /// Create a child-list retained invalidation batch.
    #[inline]
    fn new(
        queries: Vec<LightmountRetainedStyleChildListInvalidationQuery<Root>>,
        base_roots: Vec<Root>,
        empty_target_fallback_roots: Vec<Root>,
        relative_previous_sibling_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self {
            queries,
            base_roots,
            empty_target_fallback_roots,
            relative_previous_sibling_cleanup_roots,
        }
    }

    /// Consume this batch into a runtime-owned pending plan sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountRetainedStyleChildListInvalidationQueriesSink<Root>,
    ) {
        for row in self.queries {
            let (query, requirement) = row.into_query_and_requirement();
            target.record_child_list_retained_query(query, requirement);
        }
        target.extend_child_list_base_roots(self.base_roots);
        target.extend_child_list_empty_target_fallback_roots(self.empty_target_fallback_roots);
        target.extend_child_list_relative_previous_sibling_cleanup_roots(
            self.relative_previous_sibling_cleanup_roots,
        );
    }
}

impl<Root> Default for LightmountRetainedStyleChildListInvalidationQueryBuilder<Root>
where
    Root: Eq + Hash,
{
    #[inline]
    fn default() -> Self {
        Self {
            queries: IndexMap::new(),
            base_roots: IndexSet::new(),
            empty_target_fallback_roots: IndexSet::new(),
            relative_previous_sibling_cleanup_roots: IndexSet::new(),
        }
    }
}

impl<Root> LightmountRetainedStyleChildListInvalidationQueryBuilder<Root>
where
    Root: Eq + Hash,
{
    /// Create an empty child-list retained invalidation query builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert retained queries, merging the source dependency requirement for
    /// duplicate rows.
    pub fn insert_queries(
        &mut self,
        queries: impl IntoIterator<Item = LightmountRetainedStyleInvalidationQuery<Root>>,
        requirement: LightmountSourceDependencyRequestRequirement,
    ) {
        for query in queries {
            self.queries
                .entry(query)
                .and_modify(|existing| {
                    *existing = lightmount_merge_source_dependency_request_requirement(
                        *existing,
                        requirement,
                    );
                })
                .or_insert(requirement);
        }
    }

    /// Insert a base cleanup root.
    #[inline]
    pub fn insert_base_root(&mut self, root: Root) {
        self.base_roots.insert(root);
    }

    /// Insert a fallback root for empty-target structural invalidation.
    #[inline]
    pub fn insert_empty_target_fallback_root(&mut self, root: Root) {
        self.empty_target_fallback_roots.insert(root);
    }

    /// Insert a cleanup root for direct previous-sibling relative dependency
    /// handling.
    #[inline]
    pub fn insert_relative_previous_sibling_cleanup_root(&mut self, root: Root) {
        self.relative_previous_sibling_cleanup_roots.insert(root);
    }

    /// Consume the builder into a typed child-list invalidation batch.
    pub fn into_queries(self) -> Option<LightmountRetainedStyleChildListInvalidationQueries<Root>> {
        (!self.queries.is_empty()).then(|| {
            LightmountRetainedStyleChildListInvalidationQueries::new(
                self.queries
                    .into_iter()
                    .map(|(query, requirement)| {
                        LightmountRetainedStyleChildListInvalidationQuery::new(query, requirement)
                    })
                    .collect(),
                self.base_roots.into_iter().collect(),
                self.empty_target_fallback_roots.into_iter().collect(),
                self.relative_previous_sibling_cleanup_roots
                    .into_iter()
                    .collect(),
            )
        })
    }
}

impl<Root> LightmountChildListSiblingBoundaryPlan<Root> {
    #[inline]
    fn new(
        root: Root,
        include_base_root: bool,
        include_empty_target_fallback_root: bool,
        include_relative_previous_sibling_cleanup_root: bool,
    ) -> Self {
        Self {
            root,
            include_base_root,
            include_empty_target_fallback_root,
            include_relative_previous_sibling_cleanup_root,
        }
    }

    /// Return the sibling boundary root.
    #[cfg(test)]
    #[inline]
    fn root(&self) -> &Root {
        &self.root
    }

    /// Return whether this boundary contributes to base cleanup roots.
    #[cfg(test)]
    #[inline]
    fn includes_base_root(&self) -> bool {
        self.include_base_root
    }

    /// Return whether this boundary contributes to empty-target fallback roots.
    #[cfg(test)]
    #[inline]
    fn includes_empty_target_fallback_root(&self) -> bool {
        self.include_empty_target_fallback_root
    }

    /// Return whether this boundary contributes to relative previous-sibling
    /// cleanup roots.
    #[cfg(test)]
    #[inline]
    fn includes_relative_previous_sibling_cleanup_root(&self) -> bool {
        self.include_relative_previous_sibling_cleanup_root
    }

    /// Apply this plan to a child-list retained query builder.
    pub fn apply_to_builder(
        &self,
        builder: &mut LightmountRetainedStyleChildListInvalidationQueryBuilder<Root>,
    ) where
        Root: Clone + Eq + Hash,
    {
        if self.include_base_root {
            builder.insert_base_root(self.root.clone());
        }
        if self.include_empty_target_fallback_root {
            builder.insert_empty_target_fallback_root(self.root.clone());
        }
        if self.include_relative_previous_sibling_cleanup_root {
            builder.insert_relative_previous_sibling_cleanup_root(self.root.clone());
        }
    }
}

/// Return the retained cleanup bucket plan for one child-list sibling boundary.
#[inline]
pub fn lightmount_child_list_sibling_boundary_plan<Root>(
    root: Option<Root>,
    sibling_is_changed_by_mutation_batch: bool,
    kind: LightmountChildListSiblingBoundaryKind,
) -> Option<LightmountChildListSiblingBoundaryPlan<Root>> {
    if sibling_is_changed_by_mutation_batch {
        return None;
    }

    let root = root?;
    let (include_base_root, include_empty_target_fallback_root, include_relative_cleanup_root) =
        match kind {
            LightmountChildListSiblingBoundaryKind::AddedPreviousSibling { inserted_at_end } => {
                (inserted_at_end, true, true)
            },
            LightmountChildListSiblingBoundaryKind::AddedNextSibling => (true, true, false),
            LightmountChildListSiblingBoundaryKind::RemovedPreviousSibling => (true, true, true),
            LightmountChildListSiblingBoundaryKind::RemovedNextSibling => (true, true, false),
            LightmountChildListSiblingBoundaryKind::RemovedEarlierSibling => (false, false, true),
        };

    Some(LightmountChildListSiblingBoundaryPlan::new(
        root,
        include_base_root,
        include_empty_target_fallback_root,
        include_relative_cleanup_root,
    ))
}

impl<Root> LightmountRetainedStyleChildListInvalidationQuery<Root> {
    /// Create one child-list retained invalidation query row.
    #[inline]
    fn new(
        query: LightmountRetainedStyleInvalidationQuery<Root>,
        requirement: LightmountSourceDependencyRequestRequirement,
    ) -> Self {
        Self { query, requirement }
    }

    /// Consume this row into query and requirement parts.
    #[inline]
    fn into_query_and_requirement(
        self,
    ) -> (
        LightmountRetainedStyleInvalidationQuery<Root>,
        LightmountSourceDependencyRequestRequirement,
    ) {
        (self.query, self.requirement)
    }
}

/// Reason a dependency query cannot be represented as exact dependency kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LightmountDependencyFallbackReason {
    /// Stylo recorded an unknown dependency for this query.
    UnknownDependency,
    /// Stylo's invalidation map requires full-selector invalidation.
    FullSelector,
    /// Relative selector dependencies are present but not exposed precisely.
    RelativeAnySelector,
    /// Scope dependencies are present but not exposed precisely.
    ScopeDependency,
    /// State dependencies are present but not exposed precisely.
    UnsupportedStateDependency,
    /// The dependency shape cannot currently be represented by an exact retained
    /// invalidator path.
    UnsupportedDependency,
    /// `:nth-child(... of ...)` dependency exactness could not be represented
    /// by the retained invalidator's current root set.
    NthOfDependency,
    /// A selector-list dependency nested inside a relative selector cannot be
    /// represented by the retained invalidator's exact root set.
    NestedRelativeSelectorDependency,
}

/// Why a source-aware invalidation batch could not produce exact roots.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountSourceInvalidationFallbackReason {
    /// Stylo recorded an unknown dependency for this query.
    UnknownDependency,
    /// Stylo's invalidation map requires full-selector invalidation.
    FullSelector,
    /// Relative selector dependencies are present but not exposed precisely.
    RelativeAnySelector,
    /// Scope dependencies are present but not exposed precisely.
    ScopeDependency,
    /// State dependencies are present but not exposed precisely.
    UnsupportedStateDependency,
    /// Shadow dependency exactness could not be represented by this query.
    UnsupportedShadowDependency,
    /// The renderer had to use conservative fallback roots from the active
    /// source scope instead of cause-local or source-local roots.
    SourceScopeFallback,
    /// Stylo dependency metadata says the dependency cannot be represented by
    /// the exact retained invalidator path.
    UnsupportedDependency,
    /// `:nth-child(... of ...)` dependency exactness still needs reasoned
    /// fallback handling.
    NthOfDependency,
    /// A selector-list dependency nested inside a relative selector still needs
    /// reasoned fallback handling.
    NestedRelativeSelectorDependency,
    /// The retained invalidator produced no roots, but the result was not
    /// proven to be an exact no-op for this source/query batch.
    InexactEmptyResult,
    /// The batch needed source fallback roots, but none were provided by the
    /// source/scope owner.
    MissingFallbackRoots,
    /// The retained style system was unavailable when source queries were
    /// drained.
    MissingRetainedStyleSystem,
    /// The retained style system did not have per-source cascade data for this
    /// source.
    MissingRetainedCascadeData,
}

/// Return whether an attribute mutation can be represented by the retained
/// source-local invalidator.
#[inline]
pub fn lightmount_attribute_change_can_use_retained_invalidator(
    _attribute_name: &str,
    has_non_css_runtime_side_effect: bool,
) -> bool {
    !has_non_css_runtime_side_effect
}

/// Return whether an attribute mutation may avoid fallback roots once a
/// retained dependency path is available.
#[inline]
pub fn lightmount_attribute_change_can_skip_fallback_without_dependency(
    attribute_name: &str,
) -> bool {
    attribute_name == "class"
        || attribute_name == "dir"
        || attribute_name == "lang"
        || attribute_name.starts_with("data-")
        || attribute_name.starts_with("aria-")
}

/// Runtime mutation facts used to select conservative source-local fallback
/// roots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightmountRuntimeFallbackRootInput<'a, Root> {
    /// Attribute mutation fallback policy.
    Attribute {
        /// Mutated element.
        element: Root,
        /// Mutated attribute local name.
        attribute_name: &'a str,
        /// Whether Stylo dependency metadata reported a relevant dependency.
        has_dependency_change: bool,
        /// Whether this attribute also has non-CSS runtime side effects.
        has_non_css_runtime_side_effect: bool,
    },
    /// Child-list mutation fallback policy.
    ChildList {
        /// Nodes added by the mutation.
        added_nodes: &'a [Root],
    },
    /// Slot-assignment mutation fallback policy.
    SlotAssignment {
        /// Mutated slot element.
        slot: Root,
        /// Whether previous and current assigned-node snapshots are available.
        has_assignment_snapshot: bool,
    },
    /// Newly connected subtree fallback policy.
    ConnectedSubtree {
        /// Connected subtree root.
        root: Root,
    },
    /// Mutation kind that does not need source-local fallback roots.
    OtherMutation,
}

/// Runtime-owned resolver for DOM-specific fallback roots.
pub trait LightmountRuntimeFallbackRootResolver<Root> {
    /// Return the conservative fallback root for an unknown slot assignment.
    fn unknown_slot_assignment_fallback_root(&self, slot: Root) -> Root;
}

/// Build conservative source-local fallback roots with mutation-time context.
///
/// This owns the CSS-facing fallback policy and de-duplication. The runtime
/// resolver keeps DOM traversal and shadow-host lookup outside Stylo.
pub fn lightmount_runtime_fallback_roots_for_mutation_inputs<'a, Root>(
    inputs: impl IntoIterator<Item = LightmountRuntimeFallbackRootInput<'a, Root>>,
    resolver: &impl LightmountRuntimeFallbackRootResolver<Root>,
) -> Vec<Root>
where
    Root: Copy + Eq + Hash + 'a,
{
    let inputs = inputs.into_iter().collect::<Vec<_>>();
    let has_child_list_input = inputs
        .iter()
        .any(|input| matches!(input, LightmountRuntimeFallbackRootInput::ChildList { .. }));
    let all_inputs_are_child_list = has_child_list_input
        && inputs
            .iter()
            .all(|input| matches!(input, LightmountRuntimeFallbackRootInput::ChildList { .. }));
    let mut roots = IndexSet::new();

    for input in inputs {
        match input {
            LightmountRuntimeFallbackRootInput::Attribute {
                element,
                attribute_name,
                has_dependency_change,
                has_non_css_runtime_side_effect,
            } => {
                if lightmount_attribute_change_can_use_retained_invalidator(
                    attribute_name,
                    has_non_css_runtime_side_effect,
                ) && has_dependency_change
                    && lightmount_attribute_change_can_skip_fallback_without_dependency(
                        attribute_name,
                    )
                {
                    continue;
                }
                roots.insert(element);
            },
            LightmountRuntimeFallbackRootInput::ChildList { added_nodes } => {
                if !all_inputs_are_child_list {
                    roots.extend(added_nodes.iter().copied());
                }
            },
            LightmountRuntimeFallbackRootInput::SlotAssignment {
                slot,
                has_assignment_snapshot,
            } => {
                if !has_assignment_snapshot {
                    roots.insert(resolver.unknown_slot_assignment_fallback_root(slot));
                }
            },
            LightmountRuntimeFallbackRootInput::ConnectedSubtree { root } => {
                roots.insert(root);
            },
            LightmountRuntimeFallbackRootInput::OtherMutation => {},
        }
    }

    roots.into_iter().collect()
}

/// Return whether a state mutation can be represented by the retained
/// source-local invalidator.
#[inline]
pub fn lightmount_state_change_can_use_retained_invalidator(
    state: ElementState,
    old_state: Option<ElementState>,
) -> bool {
    old_state.is_some() || lightmount_retained_exact_state_change(state).is_some()
}

/// Return the source fallback reason for a state mutation that cannot use the
/// retained source-local invalidator.
#[inline]
pub fn lightmount_source_fallback_reason_for_unretained_state_change(
    state: ElementState,
    old_state: Option<ElementState>,
) -> Option<LightmountSourceInvalidationFallbackReason> {
    (!lightmount_state_change_can_use_retained_invalidator(state, old_state))
        .then_some(LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency)
}

fn lightmount_retained_exact_state_change(state: ElementState) -> Option<ElementState> {
    if state.bits().count_ones() != 1 {
        return None;
    }
    let exact_states = ElementState::CHECKED
        | ElementState::INDETERMINATE
        | ElementState::PLACEHOLDER_SHOWN
        | ElementState::DEFINED
        | ElementState::PAUSED
        | ElementState::MUTED
        | ElementState::SEEKING;
    state.intersects(exact_states).then_some(state)
}

/// Whether a retained source had fallback roots available while producing its
/// source-local result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountSourceFallbackRootAvailability {
    /// Source fallback roots were available.
    Available {
        /// Number of available fallback roots.
        root_count: usize,
    },
    /// Source fallback roots were required but unavailable.
    Missing,
}

impl LightmountSourceFallbackRootAvailability {
    /// Returns source fallback root availability for a concrete root count.
    #[inline]
    pub fn for_root_count(root_count: usize) -> Option<Self> {
        (root_count > 0).then_some(Self::Available { root_count })
    }
}

/// Merge two retained source invalidation kinds using Lightmount's source
/// result priority.
#[inline]
pub fn lightmount_merge_retained_source_invalidation_kind(
    existing: LightmountRetainedSourceStyleInvalidationKind,
    incoming: LightmountRetainedSourceStyleInvalidationKind,
) -> LightmountRetainedSourceStyleInvalidationKind {
    existing.merged_with(incoming)
}

/// Return whether this kind can be used as a fallback-root payload instead of
/// retained source-local queries.
#[inline]
pub fn lightmount_retained_source_invalidation_kind_can_use_fallback_payload(
    kind: LightmountRetainedSourceStyleInvalidationKind,
) -> bool {
    !kind.carries_retained_queries()
}

/// Merge optional fallback-root retained source kinds.
///
/// `RetainedQueries` is intentionally rejected here because this helper only
/// describes fallback-root target priority for retained-query sources.
#[inline]
pub fn lightmount_merge_retained_source_invalidation_fallback_kind(
    existing: Option<LightmountRetainedSourceStyleInvalidationKind>,
    incoming: Option<LightmountRetainedSourceStyleInvalidationKind>,
) -> Option<LightmountRetainedSourceStyleInvalidationKind> {
    let Some(incoming) = incoming else {
        return existing;
    };
    debug_assert!(
        lightmount_retained_source_invalidation_kind_can_use_fallback_payload(incoming),
        "fallback kind should describe fallback roots"
    );
    Some(match existing {
        Some(existing) => {
            debug_assert!(
                lightmount_retained_source_invalidation_kind_can_use_fallback_payload(existing),
                "fallback kind should describe fallback roots"
            );
            existing.merged_with(incoming)
        },
        None => incoming,
    })
}

/// How one retained source invalidation input was resolved.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountSourceStyleInvalidationSourceResultKind {
    /// The retained invalidator produced an exact-enough result for this source.
    Exact,
    /// The source intentionally carried fallback roots without retained queries.
    FallbackOnly,
    /// The source used mutation-context roots for a dependency that Stylo still
    /// cannot report as exact affected handles.
    ContextFallback,
    /// The source fell back to the wider source scope because no narrower
    /// cause/source-local roots could represent the invalidation safely.
    SourceScopeFallback,
    /// The batch needed source fallback roots, but none were available.
    MissingFallbackRoots,
    /// The retained style system was unavailable for this source query.
    MissingRetainedStyleSystem,
    /// The retained style system was available, but did not have per-source
    /// cascade data for this source query.
    MissingRetainedCascadeData,
    /// The source could not be answered exactly and used fallback roots, or
    /// requested a wider fallback when no source-local fallback roots were
    /// available. See fallback reasons on the source result for why.
    Fallback,
}

/// Sink used to summarize retained source-result kind categories.
pub trait LightmountSourceStyleInvalidationSourceResultKindSummarySink {
    /// Record a retained source target whose retained system or cascade data was
    /// unavailable.
    fn record_retained_source_unavailable_target(&mut self);

    /// Record a source-scope fallback target.
    fn record_source_scope_fallback_target(&mut self);

    /// Record a context-fallback target.
    fn record_context_fallback_target(&mut self);
}

/// Summary view for retained source-result kind categories.
pub trait LightmountSourceStyleInvalidationSourceResultKindSummary {
    /// Record summary counters into a runtime-owned sink.
    fn record_summary_into(
        &self,
        target: &mut impl LightmountSourceStyleInvalidationSourceResultKindSummarySink,
    );
}

impl LightmountSourceStyleInvalidationSourceResultKindSummary
    for LightmountSourceStyleInvalidationSourceResultKind
{
    #[inline]
    fn record_summary_into(
        &self,
        target: &mut impl LightmountSourceStyleInvalidationSourceResultKindSummarySink,
    ) {
        match self {
            Self::MissingRetainedStyleSystem | Self::MissingRetainedCascadeData => {
                target.record_retained_source_unavailable_target();
            },
            Self::SourceScopeFallback => {
                target.record_source_scope_fallback_target();
            },
            Self::ContextFallback => {
                target.record_context_fallback_target();
            },
            _ => {},
        }
    }
}

/// Sink used to summarize retained source fallback-root availability.
pub trait LightmountSourceFallbackRootAvailabilitySummarySink {
    /// Record a target that required fallback roots but did not have them.
    fn record_missing_fallback_roots_target(&mut self);
}

/// Summary view for retained source fallback-root availability.
pub trait LightmountSourceFallbackRootAvailabilitySummary {
    /// Record summary counters into a runtime-owned sink.
    fn record_summary_into(
        &self,
        target: &mut impl LightmountSourceFallbackRootAvailabilitySummarySink,
    );
}

impl LightmountSourceFallbackRootAvailabilitySummary for LightmountSourceFallbackRootAvailability {
    #[inline]
    fn record_summary_into(
        &self,
        target: &mut impl LightmountSourceFallbackRootAvailabilitySummarySink,
    ) {
        if matches!(self, Self::Missing) {
            target.record_missing_fallback_roots_target();
        }
    }
}

/// One retained stylesheet source input for a source-aware invalidation batch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountRetainedSourceStyleInvalidationKind {
    /// Run retained Stylo invalidation queries against this source's cascade data.
    RetainedQueries,
    /// Apply the source's fallback roots without retained queries.
    FallbackOnly,
    /// Apply mutation-context fallback roots without retained queries.
    ContextFallback,
    /// Apply fallback roots from a conservative source-scope fallback.
    SourceScopeFallback,
    /// The source required fallback roots, but none were available.
    MissingFallbackRoots,
}

impl LightmountRetainedSourceStyleInvalidationKind {
    /// Merge two fallback kinds using the conservative priority Lightmount needs
    /// when co-batching source dependency targets.
    fn merged_with(self, incoming: Self) -> Self {
        if self == Self::RetainedQueries || incoming == Self::RetainedQueries {
            return Self::RetainedQueries;
        }
        if self == Self::MissingFallbackRoots || incoming == Self::MissingFallbackRoots {
            return Self::MissingFallbackRoots;
        }
        if self == Self::SourceScopeFallback || incoming == Self::SourceScopeFallback {
            return Self::SourceScopeFallback;
        }
        if self == Self::FallbackOnly || incoming == Self::FallbackOnly {
            return Self::FallbackOnly;
        }
        if self == Self::ContextFallback || incoming == Self::ContextFallback {
            return Self::ContextFallback;
        }
        Self::FallbackOnly
    }

    /// Returns whether this kind carries retained invalidation queries.
    #[inline]
    fn carries_retained_queries(self) -> bool {
        self == Self::RetainedQueries
    }

    /// Returns whether this kind can be represented as a fallback-root target.
    #[inline]
    fn can_target_fallback_root(self) -> bool {
        matches!(self, Self::FallbackOnly | Self::SourceScopeFallback)
    }

    /// Returns the source result kind represented by this retained source kind.
    #[inline]
    fn fallback_source_result_kind(
        self,
        has_fallback_reasons: bool,
    ) -> LightmountSourceStyleInvalidationSourceResultKind {
        match self {
            Self::RetainedQueries => LightmountSourceStyleInvalidationSourceResultKind::Fallback,
            Self::FallbackOnly if has_fallback_reasons => {
                LightmountSourceStyleInvalidationSourceResultKind::Fallback
            },
            Self::FallbackOnly => LightmountSourceStyleInvalidationSourceResultKind::FallbackOnly,
            Self::ContextFallback => {
                LightmountSourceStyleInvalidationSourceResultKind::ContextFallback
            },
            Self::SourceScopeFallback => {
                LightmountSourceStyleInvalidationSourceResultKind::SourceScopeFallback
            },
            Self::MissingFallbackRoots => {
                LightmountSourceStyleInvalidationSourceResultKind::MissingFallbackRoots
            },
        }
    }

    /// Returns fallback root availability represented by this source kind and
    /// the number of fallback roots available to it.
    #[inline]
    fn fallback_root_availability(
        self,
        fallback_root_count: usize,
    ) -> Option<LightmountSourceFallbackRootAvailability> {
        if self == Self::MissingFallbackRoots {
            return Some(LightmountSourceFallbackRootAvailability::Missing);
        }
        LightmountSourceFallbackRootAvailability::for_root_count(fallback_root_count)
    }

    /// Returns the fallback reason implied directly by this kind, if any.
    #[inline]
    fn fallback_reason(self) -> Option<LightmountSourceInvalidationFallbackReason> {
        match self {
            Self::SourceScopeFallback => {
                Some(LightmountSourceInvalidationFallbackReason::SourceScopeFallback)
            },
            Self::MissingFallbackRoots => {
                Some(LightmountSourceInvalidationFallbackReason::MissingFallbackRoots)
            },
            Self::RetainedQueries | Self::FallbackOnly | Self::ContextFallback => None,
        }
    }
}

/// One retained stylesheet source input for a source-aware invalidation batch.
pub struct LightmountRetainedSourceStyleInvalidation<'a, Root, Snapshot> {
    input: LightmountRetainedSourceStyleInvalidationInput<'a, Root, Snapshot>,
}

enum LightmountRetainedSourceStyleInvalidationInput<'a, Root, Snapshot> {
    /// Run retained Stylo invalidation queries against this source's cascade
    /// data.
    RetainedQueries {
        /// Fallback-root target kind to use if retained query exactness fails.
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        /// Per-source cascade data, if available.
        cascade_data: Option<&'a ServoArc<CascadeData>>,
        /// Shadow root whose cascade data should be installed for this source.
        shadow_root: Option<Root>,
        /// Source-local retained invalidation queries.
        queries: &'a IndexSet<LightmountRetainedStyleInvalidationQuery<Root>>,
        /// Reasoned fallback roots from source/cause planning.
        reasoned_fallback_roots: &'a IndexSet<Root>,
        /// Exact-safety fallback roots used when exact query capability is
        /// unavailable.
        exact_safety_fallback_roots: &'a IndexSet<Root>,
        /// Fallback reasons already known before source-local query execution.
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
        /// Runtime-owned mutation snapshot payload.
        mutation_snapshot: &'a Snapshot,
    },
    /// Apply fallback roots without retained source-local queries.
    Fallback {
        /// Fallback source input kind.
        kind: LightmountRetainedSourceStyleInvalidationKind,
        /// Fallback roots selected by the runtime/source owner.
        fallback_roots: &'a IndexSet<Root>,
        /// Fallback reasons selected by the runtime/source owner.
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
}

impl<'a, Root, Snapshot> Copy for LightmountRetainedSourceStyleInvalidationInput<'a, Root, Snapshot> where
    Root: Copy
{
}

impl<'a, Root, Snapshot> Clone
    for LightmountRetainedSourceStyleInvalidationInput<'a, Root, Snapshot>
where
    Root: Copy,
{
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

/// Sink used by a runtime owner to execute one retained source invalidation
/// input without matching on the input variant in the runtime adapter.
pub trait LightmountRetainedSourceStyleInvalidationSink<'a, Root, Snapshot> {
    /// Run retained source-local queries.
    fn run_retained_source_style_invalidation_queries(
        &mut self,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        cascade_data: Option<&'a ServoArc<CascadeData>>,
        shadow_root: Option<Root>,
        queries: &'a IndexSet<LightmountRetainedStyleInvalidationQuery<Root>>,
        reasoned_fallback_roots: &'a IndexSet<Root>,
        exact_safety_fallback_roots: &'a IndexSet<Root>,
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
        mutation_snapshot: &'a Snapshot,
    );

    /// Apply a fallback-only source input.
    fn run_fallback_source_style_invalidation(
        &mut self,
        kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: &'a IndexSet<Root>,
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
    );
}

impl<'a, Root, Snapshot> Copy for LightmountRetainedSourceStyleInvalidation<'a, Root, Snapshot> where
    Root: Copy
{
}

impl<'a, Root, Snapshot> Clone for LightmountRetainedSourceStyleInvalidation<'a, Root, Snapshot>
where
    Root: Copy,
{
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

/// Build a retained source invalidation input from typed source parts.
#[inline]
pub fn lightmount_retained_source_style_invalidation_from_parts<'a, Root, Snapshot>(
    kind: LightmountRetainedSourceStyleInvalidationKind,
    fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
    cascade_data: Option<&'a ServoArc<CascadeData>>,
    shadow_root: Option<Root>,
    retained_queries: Option<&'a IndexSet<LightmountRetainedStyleInvalidationQuery<Root>>>,
    reasoned_fallback_roots: &'a IndexSet<Root>,
    exact_safety_fallback_roots: &'a IndexSet<Root>,
    fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
    mutation_snapshot: &'a Snapshot,
) -> LightmountRetainedSourceStyleInvalidation<'a, Root, Snapshot> {
    if kind.carries_retained_queries() {
        let queries =
            retained_queries.expect("retained source invalidation must carry retained queries");
        return LightmountRetainedSourceStyleInvalidation::retained_queries(
            fallback_kind,
            cascade_data,
            shadow_root,
            queries,
            reasoned_fallback_roots,
            exact_safety_fallback_roots,
            fallback_reasons,
            mutation_snapshot,
        );
    }

    LightmountRetainedSourceStyleInvalidation::fallback(
        kind,
        reasoned_fallback_roots,
        fallback_reasons,
    )
}

impl<'a, Root, Snapshot> LightmountRetainedSourceStyleInvalidation<'a, Root, Snapshot> {
    /// Create retained source-local query input.
    #[inline]
    fn retained_queries(
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        cascade_data: Option<&'a ServoArc<CascadeData>>,
        shadow_root: Option<Root>,
        queries: &'a IndexSet<LightmountRetainedStyleInvalidationQuery<Root>>,
        reasoned_fallback_roots: &'a IndexSet<Root>,
        exact_safety_fallback_roots: &'a IndexSet<Root>,
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
        mutation_snapshot: &'a Snapshot,
    ) -> Self {
        debug_assert!(
            !queries.is_empty(),
            "retained source invalidation must carry retained queries"
        );
        debug_assert!(
            fallback_kind.is_none_or(|kind| !kind.carries_retained_queries()),
            "retained-query fallback kind must describe fallback roots"
        );
        Self {
            input: LightmountRetainedSourceStyleInvalidationInput::RetainedQueries {
                fallback_kind,
                cascade_data,
                shadow_root,
                queries,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
                mutation_snapshot,
            },
        }
    }

    /// Create fallback-only retained source input.
    #[inline]
    fn fallback(
        kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: &'a IndexSet<Root>,
        fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        debug_assert!(
            !kind.carries_retained_queries(),
            "fallback source invalidation must not carry retained queries"
        );
        Self {
            input: LightmountRetainedSourceStyleInvalidationInput::Fallback {
                kind,
                fallback_roots,
                fallback_reasons,
            },
        }
    }

    /// Drain this source input into a runtime-owned sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountRetainedSourceStyleInvalidationSink<'a, Root, Snapshot>,
    ) {
        match self.input {
            LightmountRetainedSourceStyleInvalidationInput::RetainedQueries {
                fallback_kind,
                cascade_data,
                shadow_root,
                queries,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
                mutation_snapshot,
            } => {
                target.run_retained_source_style_invalidation_queries(
                    fallback_kind,
                    cascade_data,
                    shadow_root,
                    queries,
                    reasoned_fallback_roots,
                    exact_safety_fallback_roots,
                    fallback_reasons,
                    mutation_snapshot,
                );
            },
            LightmountRetainedSourceStyleInvalidationInput::Fallback {
                kind,
                fallback_roots,
                fallback_reasons,
            } => {
                target.run_fallback_source_style_invalidation(
                    kind,
                    fallback_roots,
                    fallback_reasons,
                );
            },
        }
    }
}

impl From<LightmountDependencyFallbackReason> for LightmountSourceInvalidationFallbackReason {
    fn from(reason: LightmountDependencyFallbackReason) -> Self {
        match reason {
            LightmountDependencyFallbackReason::UnknownDependency => Self::UnknownDependency,
            LightmountDependencyFallbackReason::FullSelector => Self::FullSelector,
            LightmountDependencyFallbackReason::RelativeAnySelector => Self::RelativeAnySelector,
            LightmountDependencyFallbackReason::ScopeDependency => Self::ScopeDependency,
            LightmountDependencyFallbackReason::UnsupportedStateDependency => {
                Self::UnsupportedStateDependency
            },
            LightmountDependencyFallbackReason::UnsupportedDependency => {
                Self::UnsupportedDependency
            },
            LightmountDependencyFallbackReason::NthOfDependency => Self::NthOfDependency,
            LightmountDependencyFallbackReason::NestedRelativeSelectorDependency => {
                Self::NestedRelativeSelectorDependency
            },
        }
    }
}

impl LightmountDependencyInvalidationAction {
    /// Apply this action to a retained dependency invalidation sink.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountDependencyInvalidationActionSink) {
        match self {
            Self::Element => target.invalidate_element(),
            Self::ElementAndDescendants => target.invalidate_element_and_descendants(),
            Self::Descendants => target.invalidate_descendants(),
            Self::Siblings => target.invalidate_siblings(),
            Self::SlottedElements => target.invalidate_slotted_elements(),
            Self::Parts => target.invalidate_parts(),
            Self::Scope(action) => target.invalidate_scope(action),
            Self::Fallback(reason) => target.invalidate_fallback(reason),
        }
    }
}

impl LightmountScopeDependencyInvalidationAction {
    /// Apply this scope action to a retained scope dependency invalidation sink.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountScopeDependencyInvalidationActionSink) {
        match self {
            Self::ImplicitScope => target.invalidate_implicit_scope(),
            Self::ForceAtSubject { force_add } => {
                target.invalidate_scope_force_at_subject(force_add)
            },
            Self::CheckNextInScope => target.invalidate_scope_check_next(),
            Self::PushByCombinator => target.invalidate_scope_by_combinator(),
        }
    }
}

impl LightmountRelativeDependencyInvalidationAction {
    /// Apply this relative traversal action to a candidate traversal sink.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountRelativeDependencyInvalidationActionSink) {
        match self {
            Self::Ancestors => target.visit_relative_ancestor_candidates(),
            Self::Parent => target.visit_relative_parent_candidate(),
            Self::PrevSibling => target.visit_relative_previous_sibling_candidate(),
            Self::EarlierSibling => target.visit_relative_earlier_sibling_candidates(),
            Self::AncestorPrevSibling => {
                target.visit_relative_ancestor_previous_sibling_candidates()
            },
            Self::AncestorEarlierSibling => {
                target.visit_relative_ancestor_earlier_sibling_candidates()
            },
        }
    }
}

/// Check whether a dependency changes when matched against old-value snapshots
/// for the candidate element.
#[inline]
pub fn lightmount_dependency_changes_anchor_with_snapshot<E>(
    dependency: &Dependency,
    element: E,
    snapshot_map: &SnapshotMap,
    matching_context: &mut MatchingContext<'_, E::Impl>,
    scope: Option<OpaqueElement>,
) -> bool
where
    E: TElement + Copy,
{
    let wrapper = ElementWrapper::new(element, snapshot_map);
    check_dependency(dependency, &element, &wrapper, matching_context, scope)
}

/// Check whether a relative dependency's outer anchor changes for a candidate.
#[inline]
pub fn lightmount_relative_dependency_changes_anchor<E>(
    dependency: &Dependency,
    candidate: E,
    scope: Option<OpaqueElement>,
    snapshot_map: &SnapshotMap,
    quirks_mode: QuirksMode,
) -> bool
where
    E: TElement + Copy,
{
    let mut selector_caches = SelectorCaches::default();
    let mut matching_context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut selector_caches,
        quirks_mode,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::Yes,
    );
    matching_context.current_host = scope;
    lightmount_dependency_changes_anchor_with_snapshot(
        dependency,
        candidate,
        snapshot_map,
        &mut matching_context,
        scope,
    )
}

/// Visit candidate elements for one relative dependency traversal action.
#[inline]
pub fn lightmount_visit_relative_dependency_candidates<E, Visit>(
    root: E,
    action: LightmountRelativeDependencyInvalidationAction,
    sibling_traversal: &SiblingTraversalMap<E>,
    visit: Visit,
) where
    E: TElement + Copy,
    Visit: FnMut(E),
{
    let mut visitor = LightmountRelativeDependencyCandidateVisitor {
        root,
        sibling_traversal,
        visit,
    };
    action.drain_into(&mut visitor);
}

struct LightmountRelativeDependencyCandidateVisitor<'a, E: TElement, Visit> {
    root: E,
    sibling_traversal: &'a SiblingTraversalMap<E>,
    visit: Visit,
}

impl<E, Visit> LightmountRelativeDependencyInvalidationActionSink
    for LightmountRelativeDependencyCandidateVisitor<'_, E, Visit>
where
    E: TElement + Copy,
    Visit: FnMut(E),
{
    fn visit_relative_ancestor_candidates(&mut self) {
        let mut current = lightmount_style_parent_element_or_host(self.root);
        while let Some(candidate) = current {
            (self.visit)(candidate);
            current = lightmount_style_parent_element_or_host(candidate);
        }
    }

    fn visit_relative_parent_candidate(&mut self) {
        if let Some(parent) = lightmount_style_parent_element_or_host(self.root) {
            (self.visit)(parent);
        }
    }

    fn visit_relative_previous_sibling_candidate(&mut self) {
        if let Some(previous) = self.sibling_traversal.prev_sibling_for(&self.root) {
            (self.visit)(previous);
        }
    }

    fn visit_relative_earlier_sibling_candidates(&mut self) {
        let mut current = self.sibling_traversal.prev_sibling_for(&self.root);
        while let Some(candidate) = current {
            (self.visit)(candidate);
            current = candidate.prev_sibling_element();
        }
    }

    fn visit_relative_ancestor_previous_sibling_candidates(&mut self) {
        let mut current = lightmount_style_parent_element_or_host(self.root);
        while let Some(parent) = current {
            if let Some(previous) = parent.prev_sibling_element() {
                (self.visit)(previous);
            }
            current = lightmount_style_parent_element_or_host(parent);
        }
    }

    fn visit_relative_ancestor_earlier_sibling_candidates(&mut self) {
        let mut current = lightmount_style_parent_element_or_host(self.root);
        while let Some(parent) = current {
            let mut sibling = parent.prev_sibling_element();
            while let Some(candidate) = sibling {
                (self.visit)(candidate);
                sibling = candidate.prev_sibling_element();
            }
            current = lightmount_style_parent_element_or_host(parent);
        }
    }
}

#[inline]
fn lightmount_style_parent_element_or_host<E>(element: E) -> Option<E>
where
    E: TElement + Copy,
{
    element.as_node().parent_element_or_host()
}

/// Mutation-boundary roots available to source dependency planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountSourceDependencyBoundaryRoots<'a, Root> {
    empty_target_fallback_roots: &'a [Root],
    relative_previous_sibling_cleanup_roots: &'a [Root],
}

impl<'a, Root> LightmountSourceDependencyBoundaryRoots<'a, Root> {
    /// Create mutation-boundary roots for source dependency planning.
    #[inline]
    pub fn new(
        empty_target_fallback_roots: &'a [Root],
        relative_previous_sibling_cleanup_roots: &'a [Root],
    ) -> Self {
        Self {
            empty_target_fallback_roots,
            relative_previous_sibling_cleanup_roots,
        }
    }
}

impl<Root> Default for LightmountSourceDependencyBoundaryRoots<'_, Root> {
    #[inline]
    fn default() -> Self {
        Self::new(&[], &[])
    }
}

/// Planned retained invalidation work for one stylesheet source in a batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountPlannedSourceDependencyInvalidation<Root> {
    source_index: usize,
    target: LightmountPlannedSourceDependencyInvalidationTarget<Root>,
    structural_boundary_cleanup_roots: Vec<Root>,
}

/// Planned source dependency invalidation target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountPlannedSourceDependencyInvalidationTarget<Root> {
    target: LightmountPlannedSourceDependencyInvalidationTargetKind<Root>,
}

/// Fork-private planned source dependency invalidation target shape.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountPlannedSourceDependencyInvalidationTargetKind<Root> {
    /// Run retained queries, optionally carrying a fallback target for
    /// dependency shapes that cannot be exact.
    RetainedQueries {
        /// Exact retained dependency queries to run for this source.
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        /// Optional fallback kind for inexact dependency branches.
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        /// Fallback roots tied to explicit dependency fallback reasons.
        reasoned_fallback_roots: Vec<Root>,
        /// Fallback roots that are safe to use if exact invalidation is
        /// unavailable.
        exact_safety_fallback_roots: Vec<Root>,
        /// Reasons why fallback handling may be needed.
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
    /// Do not run retained queries; apply fallback roots for this source.
    FallbackOnly {
        /// Fallback policy represented by this target.
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        /// Runtime roots to clear for fallback handling.
        fallback_roots: Vec<Root>,
        /// Reasons why fallback handling is required.
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
}

/// Drainable parts for a planned source dependency invalidation target.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountPlannedSourceDependencyInvalidationTargetParts<Root> {
    /// Retained-query target parts.
    RetainedQueries {
        /// Exact retained dependency queries to run for this source.
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        /// Optional fallback kind for inexact dependency branches.
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        /// Fallback roots tied to explicit dependency fallback reasons.
        reasoned_fallback_roots: Vec<Root>,
        /// Fallback roots that are safe to use if exact invalidation is
        /// unavailable.
        exact_safety_fallback_roots: Vec<Root>,
        /// Reasons why fallback handling may be needed.
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
    /// Fallback target with explicit roots.
    FallbackWithRoots {
        /// Fallback policy represented by this target.
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        /// Runtime roots to clear for fallback handling.
        fallback_roots: Vec<Root>,
        /// Reasons why fallback handling is required.
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
    /// Fallback target whose roots are unavailable.
    MissingFallbackRoots {
        /// Reasons why fallback handling is required.
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    },
}

/// Sink for planned source dependency target parts.
pub trait LightmountPlannedSourceDependencyInvalidationTargetPartsSink<Root> {
    /// Record retained-query target parts.
    fn set_planned_retained_source_dependency_target_parts(
        &mut self,
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: Vec<Root>,
        exact_safety_fallback_roots: Vec<Root>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    );

    /// Record fallback target parts with roots.
    fn set_planned_fallback_source_dependency_target_parts(
        &mut self,
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: Vec<Root>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    );

    /// Record fallback target parts when fallback roots are unavailable.
    fn set_planned_missing_fallback_roots_source_dependency_target_parts(
        &mut self,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    );
}

/// Sink for a planned source dependency invalidation row.
pub trait LightmountPlannedSourceDependencyInvalidationPartsSink<Root>:
    LightmountPlannedSourceDependencyInvalidationTargetPartsSink<Root>
{
    /// Record the stylesheet source index for this planned row.
    fn set_planned_source_dependency_source_index(&mut self, source_index: usize);

    /// Record structural-boundary cleanup roots for this planned row.
    fn set_planned_source_dependency_structural_boundary_cleanup_roots(
        &mut self,
        structural_boundary_cleanup_roots: Vec<Root>,
    );
}

/// Fallback-root invalidation target used when no source retained query target
/// is available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountPlannedFallbackRootInvalidationTarget<Root> {
    target: LightmountPlannedSourceDependencyInvalidationTarget<Root>,
}

/// Runtime source/scope fallback input for one stylesheet source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightmountStylesheetSourceScopeFallbackInput<Root> {
    /// Inline or linked stylesheet owner element.
    StylesheetOwner {
        /// Stylesheet owner handle.
        owner: Root,
    },
    /// Adopted stylesheet scoped to a document.
    DocumentAdopted {
        /// Document handle.
        document: Root,
    },
    /// Adopted stylesheet scoped to a shadow root.
    ShadowRootAdopted {
        /// Shadow root handle.
        root: Root,
    },
    /// No source/scope fallback roots are available.
    Unscoped,
}

/// DOM-backed resolver for stylesheet source/scope fallback roots.
pub trait LightmountStylesheetSourceScopeFallbackRootsResolver<Root> {
    /// Return fallback roots for a stylesheet owner element.
    fn stylesheet_owner_source_scope_fallback_roots(&self, owner: Root) -> Vec<Root>;

    /// Return fallback roots for an adopted stylesheet scoped to a document.
    fn document_source_scope_fallback_roots(&self, document: Root) -> Vec<Root>;

    /// Return fallback roots for an adopted stylesheet scoped to a shadow root.
    fn shadow_root_source_scope_fallback_roots(&self, root: Root) -> Vec<Root>;
}

/// Source-local dependency invalidation plan before it is added to a batch.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountSourceDependencyInvalidationSourcePlan<Root> {
    /// Source-local retained or fallback work. `None` means this source does
    /// not need a planned row for the current request batch.
    Work {
        /// Planned source dependency target, if this source has work.
        target: Option<LightmountPlannedSourceDependencyInvalidationTarget<Root>>,
    },
    /// The source requires fallback and no roots are available at the requested
    /// boundary.
    RequiresSourceFallback {
        /// Source-level fallback target.
        target: LightmountPlannedSourceDependencyInvalidationTarget<Root>,
    },
}

/// Drainable fallback-root invalidation target parts.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LightmountPlannedFallbackRootInvalidationTargetParts<Root> {
    fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
    fallback_roots: Vec<Root>,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
}

/// Sink for fallback-root invalidation target parts.
pub trait LightmountPlannedFallbackRootInvalidationTargetPartsSink<Root> {
    /// Record fallback-root target parts.
    fn set_planned_fallback_root_target_parts(
        &mut self,
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: Vec<Root>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    );
}

impl<Root> LightmountPlannedSourceDependencyInvalidation<Root> {
    /// Create a planned source dependency invalidation from a typed target.
    #[inline]
    fn from_target(
        source_index: usize,
        target: LightmountPlannedSourceDependencyInvalidationTarget<Root>,
        structural_boundary_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self {
            source_index,
            target,
            structural_boundary_cleanup_roots,
        }
    }

    /// Create a retained-query planned source dependency invalidation with a
    /// fallback kind for inexact dependency branches.
    #[cfg(test)]
    #[inline]
    fn retained_queries_with_fallback_kind(
        source_index: usize,
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: Vec<Root>,
        exact_safety_fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
        structural_boundary_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self::from_target(
            source_index,
            LightmountPlannedSourceDependencyInvalidationTarget::retained_queries_with_fallback_kind(
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
            ),
            structural_boundary_cleanup_roots,
        )
    }

    /// Create a fallback-only planned source dependency invalidation.
    #[cfg(test)]
    #[inline]
    fn fallback_only(
        source_index: usize,
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
        structural_boundary_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self::fallback_with_kind(
            source_index,
            LightmountRetainedSourceStyleInvalidationKind::FallbackOnly,
            fallback_roots,
            fallback_reasons,
            structural_boundary_cleanup_roots,
        )
    }

    /// Create a fallback planned source dependency invalidation with an
    /// explicit fallback kind.
    #[cfg(test)]
    #[inline]
    fn fallback_with_kind(
        source_index: usize,
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
        structural_boundary_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self::from_target(
            source_index,
            LightmountPlannedSourceDependencyInvalidationTarget::fallback_with_kind(
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            ),
            structural_boundary_cleanup_roots,
        )
    }

    /// Create a planned source dependency invalidation for unavailable fallback
    /// roots.
    #[cfg(test)]
    #[inline]
    fn missing_fallback_roots(
        source_index: usize,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
        structural_boundary_cleanup_roots: Vec<Root>,
    ) -> Self {
        Self::fallback_with_kind(
            source_index,
            LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots,
            Vec::new(),
            fallback_reasons,
            structural_boundary_cleanup_roots,
        )
    }

    /// Drain this row into a sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountPlannedSourceDependencyInvalidationPartsSink<Root>,
    ) {
        target.set_planned_source_dependency_source_index(self.source_index);
        target.set_planned_source_dependency_structural_boundary_cleanup_roots(
            self.structural_boundary_cleanup_roots,
        );
        self.target.drain_into(target);
    }
}

impl<Root> LightmountPlannedSourceDependencyInvalidationTarget<Root> {
    /// Create a target from source dependency planner work parts.
    ///
    /// Exact-safety fallback roots are only a retained-query safety net. If a
    /// source produced no exact queries, those roots become an explicit
    /// inexact-empty fallback target instead of being silently dropped.
    #[inline]
    fn from_source_dependency_work_parts(
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: Vec<Root>,
        exact_safety_fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Option<Self> {
        let mut fallback_reasons = fallback_reasons.into_iter().collect::<IndexSet<_>>();
        if exact_queries.is_empty() {
            if reasoned_fallback_roots.is_empty() && exact_safety_fallback_roots.is_empty() {
                return None;
            }
            let fallback_roots = if reasoned_fallback_roots.is_empty() {
                fallback_reasons
                    .insert(LightmountSourceInvalidationFallbackReason::InexactEmptyResult);
                exact_safety_fallback_roots
            } else {
                reasoned_fallback_roots
            };
            return Some(Self::fallback_with_kind(
                fallback_kind
                    .unwrap_or(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly),
                fallback_roots,
                fallback_reasons,
            ));
        }

        Some(Self::retained_queries_with_fallback_kind(
            exact_queries,
            fallback_kind,
            reasoned_fallback_roots,
            exact_safety_fallback_roots,
            fallback_reasons,
        ))
    }

    /// Create a source dependency fallback target from selected fallback roots.
    ///
    /// Missing roots are represented as an explicit fallback kind and reason,
    /// not as an empty generic fallback-root payload.
    #[inline]
    fn source_dependency_fallback(
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        let mut fallback_reasons = fallback_reasons.into_iter().collect::<IndexSet<_>>();
        let fallback_kind = if fallback_roots.is_empty() {
            fallback_reasons
                .insert(LightmountSourceInvalidationFallbackReason::MissingFallbackRoots);
            LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots
        } else {
            LightmountRetainedSourceStyleInvalidationKind::FallbackOnly
        };
        Self::fallback_with_kind(fallback_kind, fallback_roots, fallback_reasons)
    }

    /// Create a retained-query target with optional fallback target policy.
    #[inline]
    fn retained_queries_with_fallback_kind(
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<Root>>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: Vec<Root>,
        exact_safety_fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        debug_assert!(
            !exact_queries.is_empty(),
            "retained planned source dependency invalidation should carry exact queries"
        );
        debug_assert!(
            fallback_kind.is_none_or(|kind| !kind.carries_retained_queries()),
            "retained planned source dependency fallback kind should describe fallback roots"
        );
        Self {
            target: LightmountPlannedSourceDependencyInvalidationTargetKind::RetainedQueries {
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons: fallback_reasons.into_iter().collect(),
            },
        }
    }

    /// Create a fallback target with an explicit fallback kind.
    #[inline]
    fn fallback_with_kind(
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        debug_assert!(
            !fallback_kind.carries_retained_queries(),
            "fallback planned source dependency invalidation should not carry retained-query kind"
        );
        let mut fallback_reasons = fallback_reasons.into_iter().collect::<IndexSet<_>>();
        if let Some(fallback_reason) = fallback_kind.fallback_reason() {
            fallback_reasons.insert(fallback_reason);
        }
        Self {
            target: LightmountPlannedSourceDependencyInvalidationTargetKind::FallbackOnly {
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            },
        }
    }

    /// Consume this target into drainable parts.
    #[inline]
    fn into_parts(self) -> LightmountPlannedSourceDependencyInvalidationTargetParts<Root> {
        match self.target {
            LightmountPlannedSourceDependencyInvalidationTargetKind::RetainedQueries {
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
            } => LightmountPlannedSourceDependencyInvalidationTargetParts::RetainedQueries {
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
            },
            LightmountPlannedSourceDependencyInvalidationTargetKind::FallbackOnly {
                fallback_kind: LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots,
                fallback_roots,
                fallback_reasons,
            } => {
                debug_assert!(
                    fallback_roots.is_empty(),
                    "missing fallback roots target should not carry fallback roots"
                );
                LightmountPlannedSourceDependencyInvalidationTargetParts::MissingFallbackRoots {
                    fallback_reasons,
                }
            },
            LightmountPlannedSourceDependencyInvalidationTargetKind::FallbackOnly {
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            } => {
                debug_assert!(
                    !fallback_kind.carries_retained_queries(),
                    "fallback planned source dependency target should not carry retained-query kind"
                );
                LightmountPlannedSourceDependencyInvalidationTargetParts::FallbackWithRoots {
                    fallback_kind,
                    fallback_roots,
                    fallback_reasons,
                }
            },
        }
    }

    /// Drain this target into a sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountPlannedSourceDependencyInvalidationTargetPartsSink<Root>,
    ) {
        self.into_parts().drain_into(target);
    }
}

impl<Root> LightmountPlannedSourceDependencyInvalidationTargetParts<Root> {
    /// Drain these target parts into a sink.
    #[inline]
    fn drain_into(
        self,
        target: &mut impl LightmountPlannedSourceDependencyInvalidationTargetPartsSink<Root>,
    ) {
        match self {
            Self::RetainedQueries {
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
            } => target.set_planned_retained_source_dependency_target_parts(
                exact_queries,
                fallback_kind,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
                fallback_reasons,
            ),
            Self::FallbackWithRoots {
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            } => target.set_planned_fallback_source_dependency_target_parts(
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            ),
            Self::MissingFallbackRoots { fallback_reasons } => target
                .set_planned_missing_fallback_roots_source_dependency_target_parts(
                    fallback_reasons,
                ),
        }
    }
}

impl<Root> LightmountPlannedFallbackRootInvalidationTarget<Root> {
    /// Create a fallback-only target.
    #[inline]
    fn fallback_only(
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        Self::fallback_with_kind(
            LightmountRetainedSourceStyleInvalidationKind::FallbackOnly,
            fallback_roots,
            fallback_reasons,
        )
    }

    /// Create a source-scope fallback target.
    #[inline]
    fn source_scope_fallback(
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        Self::fallback_with_kind(
            LightmountRetainedSourceStyleInvalidationKind::SourceScopeFallback,
            fallback_roots,
            fallback_reasons,
        )
    }

    /// Create a fallback-root target with an explicit fallback kind.
    #[inline]
    fn fallback_with_kind(
        fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_roots: Vec<Root>,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        Self {
            target: LightmountPlannedSourceDependencyInvalidationTarget::fallback_with_kind(
                fallback_kind,
                fallback_roots,
                fallback_reasons,
            ),
        }
    }

    /// Consume this fallback target into drainable parts.
    #[inline]
    fn into_parts(self) -> LightmountPlannedFallbackRootInvalidationTargetParts<Root> {
        let LightmountPlannedSourceDependencyInvalidationTargetKind::FallbackOnly {
            fallback_kind,
            fallback_roots,
            fallback_reasons,
        } = self.target.target
        else {
            unreachable!("fallback-root invalidation target should not carry retained queries");
        };
        debug_assert!(
            fallback_kind.can_target_fallback_root(),
            "fallback-root target should carry fallback-only or source-scope fallback kind"
        );
        LightmountPlannedFallbackRootInvalidationTargetParts {
            fallback_kind,
            fallback_roots,
            fallback_reasons,
        }
    }

    /// Drain this fallback-root target into a sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountPlannedFallbackRootInvalidationTargetPartsSink<Root>,
    ) {
        self.into_parts().drain_into(target);
    }
}

/// Return source/scope fallback roots for a stylesheet source input.
#[inline]
pub fn lightmount_stylesheet_source_scope_fallback_roots<Root: Copy>(
    input: LightmountStylesheetSourceScopeFallbackInput<Root>,
    resolver: &impl LightmountStylesheetSourceScopeFallbackRootsResolver<Root>,
) -> Vec<Root> {
    match input {
        LightmountStylesheetSourceScopeFallbackInput::StylesheetOwner { owner } => {
            resolver.stylesheet_owner_source_scope_fallback_roots(owner)
        },
        LightmountStylesheetSourceScopeFallbackInput::DocumentAdopted { document } => {
            resolver.document_source_scope_fallback_roots(document)
        },
        LightmountStylesheetSourceScopeFallbackInput::ShadowRootAdopted { root } => {
            resolver.shadow_root_source_scope_fallback_roots(root)
        },
        LightmountStylesheetSourceScopeFallbackInput::Unscoped => Vec::new(),
    }
}

/// Create a source-scope fallback target from embedder-provided fallback roots.
#[inline]
pub fn lightmount_source_scope_fallback_plan<Root>(
    source_scope_fallback_roots: impl FnOnce() -> Vec<Root>,
    fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
) -> LightmountPlannedFallbackRootInvalidationTarget<Root> {
    LightmountPlannedFallbackRootInvalidationTarget::source_scope_fallback(
        source_scope_fallback_roots(),
        fallback_reasons,
    )
}

/// Create a generic fallback-root target from embedder-provided fallback roots.
#[inline]
pub fn lightmount_fallback_roots_plan<Root>(
    fallback_roots: Vec<Root>,
    fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
) -> LightmountPlannedFallbackRootInvalidationTarget<Root> {
    LightmountPlannedFallbackRootInvalidationTarget::fallback_only(fallback_roots, fallback_reasons)
}

/// Create a fallback target from runtime fallback roots, falling back to the
/// source-scope roots only when no narrower runtime roots are available.
#[inline]
pub fn lightmount_runtime_or_source_scope_fallback_plan<Root>(
    runtime_fallback_roots: Vec<Root>,
    source_scope_fallback_roots: impl FnOnce() -> Vec<Root>,
    fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
) -> LightmountPlannedFallbackRootInvalidationTarget<Root> {
    if runtime_fallback_roots.is_empty() {
        LightmountPlannedFallbackRootInvalidationTarget::source_scope_fallback(
            source_scope_fallback_roots(),
            fallback_reasons,
        )
    } else {
        LightmountPlannedFallbackRootInvalidationTarget::fallback_only(
            runtime_fallback_roots,
            fallback_reasons,
        )
    }
}

impl<Root> LightmountPlannedFallbackRootInvalidationTargetParts<Root> {
    /// Drain these fallback target parts into a sink.
    #[inline]
    fn drain_into(
        self,
        target: &mut impl LightmountPlannedFallbackRootInvalidationTargetPartsSink<Root>,
    ) {
        target.set_planned_fallback_root_target_parts(
            self.fallback_kind,
            self.fallback_roots,
            self.fallback_reasons,
        );
    }
}

impl<Root> LightmountSourceDependencyInvalidationSourcePlan<Root> {
    /// Create a source-local work plan.
    #[inline]
    fn work(target: Option<LightmountPlannedSourceDependencyInvalidationTarget<Root>>) -> Self {
        Self::Work { target }
    }

    /// Create a source-local plan that requires source fallback.
    #[inline]
    fn requires_source_fallback(
        target: LightmountPlannedSourceDependencyInvalidationTarget<Root>,
    ) -> Self {
        Self::RequiresSourceFallback { target }
    }
}

/// Source dependency planning result for a batch of stylesheet sources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceDependencyInvalidationBatchPlan<Root> {
    plan: LightmountSourceDependencyInvalidationBatchPlanKind<Root>,
}

/// Fork-private backing shape for source dependency batch plans.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountSourceDependencyInvalidationBatchPlanKind<Root> {
    /// At least one source has retained or fallback work, with optional
    /// fallback-root work for an empty exact target.
    Work {
        /// Planned rows for stylesheet sources participating in the batch.
        sources: Vec<LightmountPlannedSourceDependencyInvalidation<Root>>,
        /// Optional fallback-root target when no source row was planned for an
        /// empty structural target.
        boundary_fallback: Option<LightmountPlannedFallbackRootInvalidationTarget<Root>>,
    },
    /// A source dependency requires fallback and no fallback roots are
    /// available at the requested boundary.
    RequiresSourceFallback {
        /// Source row that forces source-level fallback.
        source: LightmountPlannedSourceDependencyInvalidation<Root>,
    },
}

/// Sink for a source dependency batch plan.
pub trait LightmountSourceDependencyInvalidationBatchPlanSink<Root> {
    /// Record source-local planned work with an optional empty-target boundary
    /// fallback.
    fn set_source_dependency_batch_work(
        &mut self,
        sources: Vec<LightmountPlannedSourceDependencyInvalidation<Root>>,
        boundary_fallback: Option<LightmountPlannedFallbackRootInvalidationTarget<Root>>,
    );

    /// Record the source row that requires fallback when boundary roots are
    /// unavailable.
    fn set_source_dependency_batch_requires_source_fallback(
        &mut self,
        source: LightmountPlannedSourceDependencyInvalidation<Root>,
    );
}

impl<Root> LightmountSourceDependencyInvalidationBatchPlan<Root> {
    /// Create a source dependency batch plan with source-local work.
    #[inline]
    fn work(
        sources: Vec<LightmountPlannedSourceDependencyInvalidation<Root>>,
        boundary_fallback: Option<LightmountPlannedFallbackRootInvalidationTarget<Root>>,
    ) -> Self {
        Self {
            plan: LightmountSourceDependencyInvalidationBatchPlanKind::Work {
                sources,
                boundary_fallback,
            },
        }
    }

    /// Create a source dependency batch plan that requires source fallback.
    #[inline]
    fn requires_source_fallback(
        source: LightmountPlannedSourceDependencyInvalidation<Root>,
    ) -> Self {
        Self {
            plan: LightmountSourceDependencyInvalidationBatchPlanKind::RequiresSourceFallback {
                source,
            },
        }
    }

    /// Drain this batch plan into a runtime-owned pending target sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceDependencyInvalidationBatchPlanSink<Root>,
    ) {
        match self.plan {
            LightmountSourceDependencyInvalidationBatchPlanKind::Work {
                sources,
                boundary_fallback,
            } => target.set_source_dependency_batch_work(sources, boundary_fallback),
            LightmountSourceDependencyInvalidationBatchPlanKind::RequiresSourceFallback {
                source,
            } => target.set_source_dependency_batch_requires_source_fallback(source),
        }
    }
}

/// Return source-local invalidation planning for a batch of queries against one
/// Stylo source dependency summary.
///
/// The embedder supplies DOM-backed mutation-context roots through
/// `context_roots_provider`; dependency interpretation, fallback priority,
/// exact-safety roots, and source fallback target shape stay in the
/// Lightmount-facing Stylo boundary.
fn lightmount_source_dependency_invalidation_plan<Root: Copy + Eq + Hash, ContextRootsProvider>(
    summary: &LightmountSourceDependencySummary,
    selected_fallback_roots: &[Root],
    requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    context_roots_provider: &mut ContextRootsProvider,
) -> LightmountSourceDependencyInvalidationSourcePlan<Root>
where
    ContextRootsProvider: LightmountSourceDependencyInvalidationContextRootsProvider<Root>,
{
    let mut exact_queries = Vec::new();
    let mut fallback_kind = None;
    let mut reasoned_fallback_roots = Vec::new();
    let mut reasoned_fallback_seen = HashSet::new();
    // Exact-query safety roots are separate from reasoned fallback roots:
    // they are used only when retained exact invalidation is unavailable or
    // returns an inexact result.
    let mut exact_safety_fallback_roots = Vec::new();
    let mut exact_safety_fallback_seen = HashSet::new();
    let mut fallback_reasons = IndexSet::new();
    let mut missing_fallback_root_reasons = IndexSet::new();
    for request in requests {
        let dependency = summary.query_result(request.query().as_stylo_query());
        if !dependency.has_any_dependency() {
            continue;
        }
        if request.requires_child_list_structural_dependency()
            && !summary.has_child_list_structural_boundary_dependency_for_request(request)
            && !dependency.has_relative_selector_dependency()
        {
            continue;
        }
        if request.requires_relative_previous_sibling_dependency()
            && !dependency.has_relative_previous_sibling_dependency()
        {
            continue;
        }
        if lightmount_custom_state_nth_of_dependency_needs_context_fallback(
            summary,
            request,
            &dependency,
        ) {
            if let Some(context) = request.context() {
                let context_plan = LightmountDependencyContextRootPlan::new(
                    &dependency,
                    request
                        .query()
                        .allows_direct_previous_following_sibling_fallback(),
                );
                let fallback = context_roots_provider.context_roots_for_source_dependency(
                    request.query().root(),
                    context_plan,
                    context,
                );
                let context_roots = fallback.roots();
                if !context_roots.is_empty() {
                    fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                        fallback_kind,
                        Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
                    );
                    fallback_reasons
                        .insert(LightmountSourceInvalidationFallbackReason::NthOfDependency);
                    lightmount_push_unique_roots(
                        &mut reasoned_fallback_roots,
                        &mut reasoned_fallback_seen,
                        context_roots,
                    );
                    continue;
                }
            }
        }
        if dependency.requires_fallback() {
            match dependency.source_dependency_fallback_handling() {
                LightmountDependencyFallbackHandling::ContextRoots(reasons)
                    if request.context().is_some() =>
                {
                    let context = request.context().expect("checked above");
                    let context_plan = LightmountDependencyContextRootPlan::new(
                        &dependency,
                        request
                            .query()
                            .allows_direct_previous_following_sibling_fallback(),
                    );
                    let fallback = context_roots_provider.context_roots_for_source_dependency(
                        request.query().root(),
                        context_plan,
                        context,
                    );
                    let context_roots = fallback.roots();
                    if !context_roots.is_empty() {
                        fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                            fallback_kind,
                            Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
                        );
                        fallback_reasons.extend(reasons);
                        lightmount_push_unique_roots(
                            &mut reasoned_fallback_roots,
                            &mut reasoned_fallback_seen,
                            context_roots,
                        );
                        continue;
                    }
                    if selected_fallback_roots.is_empty() {
                        missing_fallback_root_reasons.extend(reasons);
                    } else {
                        fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                            fallback_kind,
                            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly),
                        );
                        fallback_reasons.extend(reasons);
                        lightmount_push_unique_roots(
                            &mut reasoned_fallback_roots,
                            &mut reasoned_fallback_seen,
                            selected_fallback_roots,
                        );
                    }
                },
                LightmountDependencyFallbackHandling::ContextRoots(reasons)
                | LightmountDependencyFallbackHandling::SourceFallback(reasons) => {
                    if selected_fallback_roots.is_empty() {
                        missing_fallback_root_reasons.extend(reasons);
                    } else {
                        fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                            fallback_kind,
                            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly),
                        );
                        fallback_reasons.extend(reasons);
                        lightmount_push_unique_roots(
                            &mut reasoned_fallback_roots,
                            &mut reasoned_fallback_seen,
                            selected_fallback_roots,
                        );
                    }
                },
            }
            continue;
        }
        if let Some(context) = request.context() {
            let context_plan = LightmountDependencyContextRootPlan::new(
                &dependency,
                request
                    .query()
                    .allows_direct_previous_following_sibling_fallback(),
            );
            let fallback = context_roots_provider.context_roots_for_source_dependency(
                request.query().root(),
                context_plan,
                context,
            );
            if fallback.requires_source_fallback() {
                let context_roots = fallback.roots();
                let reasons = dependency.source_invalidation_fallback_reasons();
                if dependency.has_only_direct_relative_previous_sibling_dependency()
                    && !context_roots.is_empty()
                {
                    fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                        fallback_kind,
                        Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
                    );
                    fallback_reasons.extend(reasons);
                    lightmount_push_unique_roots(
                        &mut reasoned_fallback_roots,
                        &mut reasoned_fallback_seen,
                        context_roots,
                    );
                } else if selected_fallback_roots.is_empty() {
                    missing_fallback_root_reasons.extend(reasons);
                } else {
                    fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                        fallback_kind,
                        Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly),
                    );
                    fallback_reasons.extend(reasons);
                    lightmount_push_unique_roots(
                        &mut reasoned_fallback_roots,
                        &mut reasoned_fallback_seen,
                        selected_fallback_roots,
                    );
                }
                continue;
            }
            let context_roots = fallback.into_roots();
            if dependency.requires_structural_context_fallback_cleanup(
                request.requires_child_list_structural_dependency(),
                request.query().is_universal(),
            ) {
                if context_roots.is_empty() {
                    missing_fallback_root_reasons
                        .insert(LightmountSourceInvalidationFallbackReason::InexactEmptyResult);
                    continue;
                }
                // This request is intentionally fallback-only: the context
                // roots are the cleanup target for structural relative
                // dependencies whose exact query would otherwise report an
                // inexact empty result. Other co-batched queries must remain
                // in the exact-query set because these fallback roots are not
                // required to subsume unrelated custom-state or media-state
                // query targets.
                fallback_kind = lightmount_merge_retained_source_invalidation_fallback_kind(
                    fallback_kind,
                    Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
                );
                fallback_reasons
                    .insert(LightmountSourceInvalidationFallbackReason::InexactEmptyResult);
                lightmount_push_unique_roots(
                    &mut reasoned_fallback_roots,
                    &mut reasoned_fallback_seen,
                    &context_roots,
                );
                continue;
            }
            lightmount_push_unique_roots(
                &mut exact_safety_fallback_roots,
                &mut exact_safety_fallback_seen,
                &context_roots,
            );
        } else if !selected_fallback_roots.is_empty() {
            lightmount_push_unique_roots(
                &mut exact_safety_fallback_roots,
                &mut exact_safety_fallback_seen,
                selected_fallback_roots,
            );
        }
        exact_queries.push((*request.query()).clone());
    }
    if !missing_fallback_root_reasons.is_empty() {
        return lightmount_source_dependency_requires_source_fallback_plan(
            selected_fallback_roots,
            missing_fallback_root_reasons,
        );
    }
    LightmountSourceDependencyInvalidationSourcePlan::work(
        LightmountPlannedSourceDependencyInvalidationTarget::from_source_dependency_work_parts(
            exact_queries,
            fallback_kind,
            reasoned_fallback_roots,
            exact_safety_fallback_roots,
            fallback_reasons,
        ),
    )
}

/// Build source-local invalidation plans for all stylesheet sources that can be
/// affected by a Lightmount mutation.
///
/// The embedder owns source scopes and DOM traversal; this planner owns source
/// dependency interpretation, target normalization, empty-target fallback, and
/// structural-boundary cleanup selection.
pub fn lightmount_source_dependency_invalidation_batch_plan<
    Root: Copy + Eq + Hash,
    ContextRootsProvider,
>(
    sources: &[LightmountSourceDependencyInvalidationBatchSource<'_, Root>],
    requests: &[LightmountSourceDependencyInvalidationRequest<'_, Root>],
    boundary_roots: LightmountSourceDependencyBoundaryRoots<'_, Root>,
    context_roots_provider: &mut ContextRootsProvider,
) -> LightmountSourceDependencyInvalidationBatchPlan<Root>
where
    ContextRootsProvider: LightmountSourceDependencyInvalidationContextRootsProvider<Root>,
{
    let mut planned_sources = Vec::new();
    let mut empty_target_fallback_source: Option<(usize, Vec<Root>)> = None;
    for (source_index, source) in sources.iter().enumerate() {
        let selected_fallback_roots = source.selected_fallback_roots();
        if source
            .dependency_summary()
            .requires_empty_target_fallback_for_requests(requests)
        {
            let has_fallback_roots = !selected_fallback_roots.is_empty();
            let should_replace_empty_target_source = match &empty_target_fallback_source {
                None => true,
                Some((_, roots)) => roots.is_empty() && has_fallback_roots,
            };
            if should_replace_empty_target_source {
                empty_target_fallback_source =
                    Some((source_index, selected_fallback_roots.to_vec()));
            }
        }
        match lightmount_source_dependency_invalidation_plan(
            source.dependency_summary(),
            selected_fallback_roots,
            requests,
            context_roots_provider,
        ) {
            LightmountSourceDependencyInvalidationSourcePlan::Work { target } => {
                let Some(target) = target else {
                    continue;
                };
                let structural_boundary_cleanup_roots = source
                    .dependency_summary()
                    .structural_boundary_cleanup_roots_for_requests(
                        requests,
                        boundary_roots.relative_previous_sibling_cleanup_roots,
                    );
                let planned_source = LightmountPlannedSourceDependencyInvalidation::from_target(
                    source_index,
                    target,
                    structural_boundary_cleanup_roots,
                );
                planned_sources.push(planned_source);
            },
            LightmountSourceDependencyInvalidationSourcePlan::RequiresSourceFallback { target } => {
                return LightmountSourceDependencyInvalidationBatchPlan::requires_source_fallback(
                    LightmountPlannedSourceDependencyInvalidation::from_target(
                        source_index,
                        target,
                        Vec::new(),
                    ),
                );
            },
        }
    }
    let boundary_fallback = match empty_target_fallback_source {
        Some((source_index, selected_fallback_roots)) => {
            if boundary_roots.empty_target_fallback_roots.is_empty() {
                return LightmountSourceDependencyInvalidationBatchPlan::requires_source_fallback(
                        LightmountPlannedSourceDependencyInvalidation::from_target(
                            source_index,
                            LightmountPlannedSourceDependencyInvalidationTarget::source_dependency_fallback(
                                selected_fallback_roots,
                                [LightmountSourceInvalidationFallbackReason::InexactEmptyResult],
                            ),
                            Vec::new(),
                        ),
                    );
            }
            Some(
                LightmountPlannedFallbackRootInvalidationTarget::fallback_only(
                    boundary_roots.empty_target_fallback_roots.to_vec(),
                    [LightmountSourceInvalidationFallbackReason::InexactEmptyResult],
                ),
            )
        },
        None => None,
    };
    LightmountSourceDependencyInvalidationBatchPlan::work(planned_sources, boundary_fallback)
}

fn lightmount_source_dependency_requires_source_fallback_plan<Root: Copy>(
    selected_fallback_roots: &[Root],
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
) -> LightmountSourceDependencyInvalidationSourcePlan<Root> {
    LightmountSourceDependencyInvalidationSourcePlan::requires_source_fallback(
        LightmountPlannedSourceDependencyInvalidationTarget::source_dependency_fallback(
            selected_fallback_roots.to_vec(),
            fallback_reasons,
        ),
    )
}

fn lightmount_custom_state_nth_of_dependency_needs_context_fallback<Root: Copy>(
    summary: &LightmountSourceDependencySummary,
    request: &LightmountSourceDependencyInvalidationRequest<'_, Root>,
    dependency: &LightmountDependencyQueryResult,
) -> bool {
    matches!(
        request.query().as_stylo_query(),
        LightmountStyleInvalidationQuery::CustomState(_)
    ) && summary.has_child_list_structural_dependency()
        && dependency.has_sibling_dependency()
        && !dependency.requires_fallback()
}

fn lightmount_push_unique_roots<Root: Copy + Eq + Hash>(
    roots: &mut Vec<Root>,
    seen: &mut HashSet<Root>,
    incoming: &[Root],
) {
    for &root in incoming {
        if seen.insert(root) {
            roots.push(root);
        }
    }
}

/// Result for one source-local Lightmount retained style invalidation query.
///
/// The DOM adapter still supplies concrete roots, but exactness, matched
/// dependency counts, fallback reasons, and merge behavior are Stylo-facing
/// query semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationQueryResult<Root> {
    affected_roots: Vec<Root>,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
}

/// Builder for one source-local retained style invalidation query result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationQueryResultBuilder<Root: Eq + Hash> {
    affected_roots: Vec<Root>,
    affected_root_set: HashSet<Root>,
    empty_result_is_exact: bool,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
}

/// Snapshot-relative affected roots and verification state for one retained
/// query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSnapshotRelativeDependencyRoots<Root> {
    roots: Vec<Root>,
    verified_dependency_count: usize,
}

/// Policy for normal retained invalidation after snapshot-relative roots have
/// been collected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountNormalStyleInvalidationDependencyPlan {
    drop_relative_dependencies: bool,
    empty_result_is_exact: bool,
}

/// Policy for relative retained invalidation after snapshot-relative roots have
/// been collected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightmountRelativeStyleInvalidationDependencyPlan {
    empty_result_is_exact: bool,
}

impl<Root> Default for LightmountSnapshotRelativeDependencyRoots<Root> {
    #[inline]
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            verified_dependency_count: 0,
        }
    }
}

impl<Root> Default for LightmountSourceStyleInvalidationQueryResult<Root> {
    #[inline]
    fn default() -> Self {
        Self {
            affected_roots: Vec::new(),
            empty_result_is_exact: false,
            matched_dependency_count: 0,
            fallback_reasons: IndexSet::new(),
        }
    }
}

impl<Root> Default for LightmountSourceStyleInvalidationQueryResultBuilder<Root>
where
    Root: Eq + Hash,
{
    #[inline]
    fn default() -> Self {
        Self {
            affected_roots: Vec::new(),
            affected_root_set: HashSet::new(),
            empty_result_is_exact: true,
            fallback_reasons: IndexSet::new(),
        }
    }
}

/// Internal source-local invalidation result after a batch of Lightmount
/// retained dependency queries has been merged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationResult<Root> {
    affected_roots: Vec<Root>,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
    fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
}

/// Drainable parts for one classified source-local invalidation result.
pub struct LightmountSourceStyleInvalidationResultParts<Root> {
    affected_roots: Vec<Root>,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
    fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
}

/// Sink used to drain source-local invalidation result policy into its owner.
pub trait LightmountSourceStyleInvalidationResultSink<Root> {
    /// Record a fully classified source-local invalidation result artifact.
    fn set_source_style_invalidation_result(
        &mut self,
        parts: LightmountSourceStyleInvalidationResultParts<Root>,
    );
}

/// Sink used by diagnostics and tests that need source-local result parts.
pub trait LightmountSourceStyleInvalidationResultPartsSink<Root> {
    /// Record the classified source-local invalidation result parts.
    fn set_source_style_invalidation_result_parts(
        &mut self,
        affected_roots: Vec<Root>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
    );
}

impl<Root> LightmountSourceStyleInvalidationQueryResult<Root> {
    /// Construct an exact empty result for a query whose matched dependencies
    /// have already been accounted for by the fork-owned dependency plan.
    #[inline]
    pub fn exact_empty(matched_dependency_count: usize) -> Self {
        Self {
            affected_roots: Vec::new(),
            empty_result_is_exact: true,
            matched_dependency_count,
            fallback_reasons: IndexSet::new(),
        }
    }

    /// Construct a single-query invalidation result from already-classified
    /// parts.
    #[inline]
    fn from_parts(
        affected_roots: Vec<Root>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        Self {
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons: fallback_reasons.into_iter().collect(),
        }
    }

    /// Consume this query result and drain affected roots into a runtime-owned
    /// root collection.
    #[inline]
    pub fn drain_affected_roots_into(self, target: &mut impl Extend<Root>) {
        target.extend(self.affected_roots);
    }
}

impl<Root> LightmountSourceStyleInvalidationResultParts<Root> {
    #[inline]
    fn from_result(result: LightmountSourceStyleInvalidationResult<Root>) -> Self {
        Self {
            affected_roots: result.affected_roots,
            fallback_reasons: result.fallback_reasons,
            fallback_kind: result.fallback_kind,
            fallback_root_availability: result.fallback_root_availability,
            empty_result_is_exact: result.empty_result_is_exact,
            matched_dependency_count: result.matched_dependency_count,
        }
    }

    /// Drain this artifact into a result-parts sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationResultPartsSink<Root>,
    ) {
        target.set_source_style_invalidation_result_parts(
            self.affected_roots,
            self.fallback_reasons,
            self.fallback_kind,
            self.fallback_root_availability,
            self.empty_result_is_exact,
            self.matched_dependency_count,
        );
    }
}

impl<Root> LightmountSourceStyleInvalidationQueryResultBuilder<Root>
where
    Root: Copy + Eq + Hash,
{
    /// Create an empty query result builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an affected root while preserving first-seen order.
    #[inline]
    pub fn note_affected_root(&mut self, root: Root) {
        lightmount_push_unique_root(&mut self.affected_roots, &mut self.affected_root_set, root);
    }

    /// Record several affected roots while preserving first-seen order.
    #[inline]
    pub fn extend_affected_roots(&mut self, roots: impl IntoIterator<Item = Root>) {
        for root in roots {
            self.note_affected_root(root);
        }
    }

    /// Return whether any affected roots have been recorded.
    #[inline]
    pub fn has_affected_roots(&self) -> bool {
        !self.affected_roots.is_empty()
    }

    /// Record whether the current dependency can prove an empty result exact.
    #[inline]
    pub fn note_empty_result_supported(&mut self, supported: bool) {
        self.empty_result_is_exact &= supported;
    }

    /// Record a fallback reason for this query.
    #[inline]
    pub fn note_fallback_reason(&mut self, reason: LightmountSourceInvalidationFallbackReason) {
        self.fallback_reasons.insert(reason);
    }

    /// Consume this builder into a typed single-query result.
    #[inline]
    pub fn into_query_result(
        self,
        matched_dependency_count: usize,
    ) -> LightmountSourceStyleInvalidationQueryResult<Root> {
        LightmountSourceStyleInvalidationQueryResult::from_parts(
            self.affected_roots,
            self.empty_result_is_exact,
            matched_dependency_count,
            self.fallback_reasons,
        )
    }
}

/// Runtime-provided mapping from a Stylo element to the retained invalidation
/// root stored in Lightmount's source-local result.
pub trait LightmountStyleInvalidationElementRoot<E, Root>
where
    E: TElement + Copy,
{
    /// Return the Lightmount root represented by a Stylo invalidated element.
    fn root_for_style_invalidation_element(&self, element: E) -> Root;
}

/// Lightmount-facing invalidation processor for Servo's tree invalidator.
///
/// The embedder supplies concrete elements, snapshots, and a root mapper. This
/// processor owns selector dependency action application, retained-vs-fallback
/// effect classification, affected-root collection, and fallback reason recording.
pub struct LightmountStyleInvalidationProcessor<'a, 'b, E, Root, RootMapper>
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    result_builder: LightmountSourceStyleInvalidationQueryResultBuilder<Root>,
    matching_context: MatchingContext<'b, E::Impl>,
    traversal_map: SiblingTraversalMap<E>,
    dependencies: Vec<&'a Dependency>,
    snapshot_map: Option<&'b SnapshotMap>,
    root_mapper: RootMapper,
}

impl<'a, 'b, E, Root, RootMapper> LightmountStyleInvalidationProcessor<'a, 'b, E, Root, RootMapper>
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    /// Create a Lightmount retained invalidation processor from already prepared
    /// Stylo invalidator inputs.
    #[inline]
    pub fn new(
        matching_context: MatchingContext<'b, E::Impl>,
        traversal_map: SiblingTraversalMap<E>,
        dependencies: Vec<&'a Dependency>,
        snapshot_map: Option<&'b SnapshotMap>,
        root_mapper: RootMapper,
    ) -> Self {
        Self {
            result_builder: LightmountSourceStyleInvalidationQueryResultBuilder::new(),
            matching_context,
            traversal_map,
            dependencies,
            snapshot_map,
            root_mapper,
        }
    }

    /// Record an already computed affected root.
    #[inline]
    pub fn note_affected_root(&mut self, root: Root) {
        self.result_builder.note_affected_root(root);
    }

    /// Consume this processor into one source-local query result.
    #[inline]
    pub fn into_query_result(
        self,
        matched_dependency_count: usize,
    ) -> LightmountSourceStyleInvalidationQueryResult<Root> {
        self.result_builder
            .into_query_result(matched_dependency_count)
    }

    fn note_affected_element(&mut self, element: E) {
        let root = self
            .root_mapper
            .root_for_style_invalidation_element(element);
        self.note_affected_root(root);
    }

    fn note_dependency(
        &mut self,
        element: E,
        dependency: &'a Dependency,
        descendant_invalidations: &mut DescendantInvalidationLists<'a>,
        sibling_invalidations: &mut InvalidationVector<'a>,
    ) -> bool {
        let mut application = LightmountDependencyInvalidationActionApplication {
            processor: self,
            element,
            dependency,
            descendant_invalidations,
            sibling_invalidations,
            invalidates_self: false,
        };
        lightmount_dependency_invalidation_action(dependency).drain_into(&mut application);
        application.invalidates_self
    }
}

impl<'a, 'b, E, Root, RootMapper> Extend<Root>
    for LightmountStyleInvalidationProcessor<'a, 'b, E, Root, RootMapper>
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    #[inline]
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = Root>,
    {
        self.result_builder.extend_affected_roots(iter);
    }
}

impl<'a, 'b, E, Root, RootMapper> InvalidationProcessor<'a, 'b, E>
    for LightmountStyleInvalidationProcessor<'a, 'b, E, Root, RootMapper>
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    fn invalidates_on_pseudo_element(&self) -> bool {
        true
    }

    fn check_outer_dependency(
        &mut self,
        dependency: &Dependency,
        element: E,
        scope: Option<OpaqueElement>,
    ) -> bool {
        let Some(snapshot_map) = self.snapshot_map else {
            return true;
        };
        lightmount_dependency_changes_anchor_with_snapshot(
            dependency,
            element,
            snapshot_map,
            &mut self.matching_context,
            scope,
        )
    }

    fn matching_context(&mut self) -> &mut MatchingContext<'b, E::Impl> {
        &mut self.matching_context
    }

    fn sibling_traversal_map(&self) -> &SiblingTraversalMap<E> {
        &self.traversal_map
    }

    fn collect_invalidations(
        &mut self,
        element: E,
        _self_invalidations: &mut InvalidationVector<'a>,
        descendant_invalidations: &mut DescendantInvalidationLists<'a>,
        sibling_invalidations: &mut InvalidationVector<'a>,
    ) -> bool {
        let mut invalidates_self = false;
        for dependency in self.dependencies.clone() {
            let empty_result_is_exact =
                match lightmount_retained_processor_dependency_effect(dependency) {
                    LightmountRetainedProcessorDependencyEffect::Retained {
                        empty_result_is_exact,
                    } => empty_result_is_exact,
                    LightmountRetainedProcessorDependencyEffect::Fallback(reason) => {
                        self.result_builder.note_fallback_reason(reason);
                        continue;
                    },
                };
            self.result_builder
                .note_empty_result_supported(empty_result_is_exact);
            invalidates_self |= self.note_dependency(
                element,
                dependency,
                descendant_invalidations,
                sibling_invalidations,
            );
        }
        if invalidates_self {
            self.note_affected_element(element);
        }
        invalidates_self
    }

    fn should_process_descendants(&mut self, _element: E) -> bool {
        true
    }

    fn recursion_limit_exceeded(&mut self, element: E) {
        self.note_affected_element(element);
    }

    fn invalidated_self(&mut self, element: E) {
        self.note_affected_element(element);
    }

    fn invalidated_sibling(&mut self, sibling: E, _of: E) {
        self.note_affected_element(sibling);
    }

    fn invalidated_descendants(&mut self, _element: E, child: E) {
        self.note_affected_element(child);
    }

    fn found_relative_selector_invalidation(
        &mut self,
        _element: E,
        kind: RelativeDependencyInvalidationKind,
        relative_dependency: &'a Dependency,
    ) {
        self.result_builder.note_fallback_reason(
            lightmount_relative_selector_invalidation_fallback_reason(kind, relative_dependency),
        );
    }
}

struct LightmountDependencyInvalidationActionApplication<
    'processor,
    'a,
    'b,
    'vectors,
    E,
    Root,
    RootMapper,
> where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    processor: &'processor mut LightmountStyleInvalidationProcessor<'a, 'b, E, Root, RootMapper>,
    element: E,
    dependency: &'a Dependency,
    descendant_invalidations: &'vectors mut DescendantInvalidationLists<'a>,
    sibling_invalidations: &'vectors mut InvalidationVector<'a>,
    invalidates_self: bool,
}

impl<'processor, 'a, 'b, 'vectors, E, Root, RootMapper>
    LightmountDependencyInvalidationActionApplication<
        'processor,
        'a,
        'b,
        'vectors,
        E,
        Root,
        RootMapper,
    >
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    fn invalidation(&self) -> Invalidation<'a> {
        Invalidation::new(
            self.dependency,
            self.processor.matching_context.current_host,
            self.processor.matching_context.scope_element,
        )
    }
}

impl<'processor, 'a, 'b, 'vectors, E, Root, RootMapper> LightmountDependencyInvalidationActionSink
    for LightmountDependencyInvalidationActionApplication<
        'processor,
        'a,
        'b,
        'vectors,
        E,
        Root,
        RootMapper,
    >
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    fn invalidate_element(&mut self) {
        self.invalidates_self = true;
    }

    fn invalidate_element_and_descendants(&mut self) {
        self.descendant_invalidations
            .dom_descendants
            .push(self.invalidation());
        self.invalidates_self = true;
    }

    fn invalidate_descendants(&mut self) {
        self.descendant_invalidations
            .dom_descendants
            .push(self.invalidation());
    }

    fn invalidate_siblings(&mut self) {
        self.sibling_invalidations.push(self.invalidation());
    }

    fn invalidate_slotted_elements(&mut self) {
        self.descendant_invalidations
            .slotted_descendants
            .push(self.invalidation());
    }

    fn invalidate_parts(&mut self) {
        self.descendant_invalidations
            .parts
            .push(self.invalidation());
    }

    fn invalidate_fallback(&mut self, reason: LightmountSourceInvalidationFallbackReason) {
        self.processor.result_builder.note_fallback_reason(reason);
    }

    fn invalidate_scope(&mut self, action: LightmountScopeDependencyInvalidationAction) {
        action.drain_into(self);
    }
}

impl<'processor, 'a, 'b, 'vectors, E, Root, RootMapper>
    LightmountScopeDependencyInvalidationActionSink
    for LightmountDependencyInvalidationActionApplication<
        'processor,
        'a,
        'b,
        'vectors,
        E,
        Root,
        RootMapper,
    >
where
    E: TElement + Copy,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
{
    fn invalidate_implicit_scope(&mut self) {
        if let Some(next) = self.dependency.next.as_ref() {
            for dep in next.as_ref().slice() {
                self.descendant_invalidations.dom_descendants.push(
                    Invalidation::new_always_effective_for_next_descendant(
                        dep,
                        self.processor.matching_context.current_host,
                        self.processor.matching_context.scope_element,
                    ),
                );
            }
        }
    }

    fn invalidate_scope_force_at_subject(&mut self, force_add: bool) {
        self.descendant_invalidations.dom_descendants.extend(
            note_scope_dependency_force_at_subject(
                self.dependency,
                self.processor.matching_context.current_host,
                self.processor.matching_context.scope_element,
                force_add,
            ),
        );
        self.invalidates_self = true;
    }

    fn invalidate_scope_check_next(&mut self) {
        let Some(next) = self.dependency.next.as_ref() else {
            return;
        };
        let scope = Some(self.element.opaque());
        for dep in next.as_ref().slice() {
            if self
                .processor
                .check_outer_dependency(dep, self.element, scope)
            {
                self.invalidates_self |= self.processor.note_dependency(
                    self.element,
                    dep,
                    self.descendant_invalidations,
                    self.sibling_invalidations,
                );
            }
        }
    }

    fn invalidate_scope_by_combinator(&mut self) {
        let invalidation = self.invalidation();
        if invalidation.combinator_to_right().is_sibling() {
            self.sibling_invalidations.push(invalidation);
        } else {
            self.descendant_invalidations
                .dom_descendants
                .push(invalidation);
        }
        self.invalidates_self = true;
    }
}

impl<Root> LightmountSnapshotRelativeDependencyRoots<Root> {
    /// Create snapshot-relative roots from already-collected DOM roots and the
    /// number of relative dependencies that were verified by snapshots.
    #[inline]
    pub fn new(roots: Vec<Root>, verified_dependency_count: usize) -> Self {
        Self {
            roots,
            verified_dependency_count,
        }
    }

    /// Snapshot-relative roots collected for this query.
    #[inline]
    fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Return whether every matched dependency was verified by snapshots.
    #[inline]
    fn verified_all_dependencies(
        &self,
        matched_dependency_count: usize,
        dependency_count: usize,
    ) -> bool {
        dependency_count != 0
            && self.verified_dependency_count == dependency_count
            && matched_dependency_count == dependency_count
    }

    /// Return whether every collected dependency in one dependency subset was
    /// verified by snapshots.
    #[inline]
    fn verified_all_collected_dependencies(&self, dependency_count: usize) -> bool {
        dependency_count != 0 && self.verified_dependency_count == dependency_count
    }

    /// Return whether a relative query's empty result can be treated as exact.
    #[inline]
    fn empty_result_is_exact(
        &self,
        matched_dependency_count: usize,
        dependency_count: usize,
        has_affected_roots: bool,
    ) -> bool {
        matched_dependency_count == 0
            || has_affected_roots
            || self.verified_all_dependencies(matched_dependency_count, dependency_count)
    }

    /// Consume this snapshot-relative result and drain affected roots into an
    /// invalidation root collection.
    #[inline]
    pub fn drain_affected_roots_into(self, target: &mut impl Extend<Root>) {
        target.extend(self.roots);
    }
}

impl LightmountNormalStyleInvalidationDependencyPlan {
    /// Drain this plan into an adapter-owned normal invalidation action sink.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountNormalStyleInvalidationDependencyPlanSink) {
        if self.should_drop_relative_dependencies() {
            target.drop_collected_relative_dependencies();
        }
        if self.empty_result_is_exact() {
            target.record_exact_empty_result();
        }
    }

    /// Return whether normal invalidation should drop collected relative
    /// dependencies before running the normal invalidator.
    #[inline]
    fn should_drop_relative_dependencies(&self) -> bool {
        self.drop_relative_dependencies
    }

    /// Return whether the normal invalidation query is an exact empty result.
    #[inline]
    fn empty_result_is_exact(&self) -> bool {
        self.empty_result_is_exact
    }
}

impl LightmountRelativeStyleInvalidationDependencyPlan {
    /// Return whether the relative invalidation query is an exact empty result.
    #[inline]
    fn empty_result_is_exact(&self) -> bool {
        self.empty_result_is_exact
    }
}

/// Sink for normal invalidation dependency plan actions.
pub trait LightmountNormalStyleInvalidationDependencyPlanSink {
    /// Drop relative dependencies collected by the normal invalidator before
    /// running it.
    fn drop_collected_relative_dependencies(&mut self);

    /// Record that this normal invalidation query can return exact empty.
    fn record_exact_empty_result(&mut self);
}

/// Return normal invalidation dependency policy after snapshot-relative
/// dependency collection.
#[inline]
pub fn lightmount_normal_style_invalidation_dependency_plan<Root>(
    query: LightmountStyleInvalidationQuery<'_>,
    matched_dependency_count: usize,
    relative_dependency_count: usize,
    snapshot_relative_roots: &LightmountSnapshotRelativeDependencyRoots<Root>,
) -> LightmountNormalStyleInvalidationDependencyPlan {
    let drop_relative_dependencies = query.drops_collected_relative_dependencies()
        || snapshot_relative_roots.verified_all_collected_dependencies(relative_dependency_count);
    let remaining_dependency_count = if drop_relative_dependencies {
        matched_dependency_count.saturating_sub(relative_dependency_count)
    } else {
        matched_dependency_count
    };
    LightmountNormalStyleInvalidationDependencyPlan {
        drop_relative_dependencies,
        empty_result_is_exact: remaining_dependency_count == 0
            && snapshot_relative_roots.roots().is_empty(),
    }
}

/// Return relative invalidation dependency policy after the relative invalidator
/// and snapshot-relative dependency collection have both run.
#[inline]
pub fn lightmount_relative_style_invalidation_dependency_plan<Root>(
    matched_dependency_count: usize,
    relative_dependency_count: usize,
    has_affected_roots: bool,
    snapshot_relative_roots: &LightmountSnapshotRelativeDependencyRoots<Root>,
) -> LightmountRelativeStyleInvalidationDependencyPlan {
    LightmountRelativeStyleInvalidationDependencyPlan {
        empty_result_is_exact: snapshot_relative_roots.empty_result_is_exact(
            matched_dependency_count,
            relative_dependency_count,
            has_affected_roots,
        ),
    }
}

/// Build one relative invalidation query result from Servo relative invalidator
/// affected roots and snapshot-relative roots.
#[inline]
pub fn lightmount_relative_style_invalidation_query_result<Root>(
    direct_affected_roots: impl IntoIterator<Item = Root>,
    snapshot_relative_roots: &LightmountSnapshotRelativeDependencyRoots<Root>,
    matched_dependency_count: usize,
    relative_dependency_count: usize,
) -> LightmountSourceStyleInvalidationQueryResult<Root>
where
    Root: Copy + Eq + Hash,
{
    let mut result_builder = LightmountSourceStyleInvalidationQueryResultBuilder::new();
    result_builder.extend_affected_roots(direct_affected_roots);
    result_builder.extend_affected_roots(snapshot_relative_roots.roots().iter().copied());
    let dependency_plan = lightmount_relative_style_invalidation_dependency_plan(
        matched_dependency_count,
        relative_dependency_count,
        result_builder.has_affected_roots(),
        snapshot_relative_roots,
    );
    result_builder.note_empty_result_supported(dependency_plan.empty_result_is_exact());
    result_builder.into_query_result(matched_dependency_count)
}

/// Run Servo's relative selector invalidator for one source-local Lightmount
/// query and return the fork-owned query result.
#[inline]
pub fn lightmount_collect_relative_style_invalidation_query_result<
    'a,
    'b,
    E,
    Root,
    RootMapper,
    SnapshotRelativeRoots,
>(
    root_mapper: RootMapper,
    element: E,
    stylist: &'a Stylist,
    query: LightmountStyleInvalidationQuery<'_>,
    quirks_mode: QuirksMode,
    snapshot_table: Option<&'b SnapshotMap>,
    sibling_traversal_map: SiblingTraversalMap<E>,
    collect_snapshot_relative_roots: SnapshotRelativeRoots,
) -> LightmountSourceStyleInvalidationQueryResult<Root>
where
    E: TElement + Copy + 'a,
    Root: Copy + Eq + Hash,
    RootMapper: LightmountStyleInvalidationElementRoot<E, Root>,
    SnapshotRelativeRoots: FnOnce(
        &[(Option<OpaqueElement>, &'a Dependency)],
    ) -> LightmountSnapshotRelativeDependencyRoots<Root>,
{
    let mut matched_dependency_count = 0;
    let mut snapshot_relative_dependencies = Vec::new();
    let affected_roots = RefCell::new(Vec::new());

    {
        let collect_affected_root = |affected: E| {
            affected_roots
                .borrow_mut()
                .push(root_mapper.root_for_style_invalidation_element(affected));
        };

        let invalidator = RelativeSelectorInvalidator {
            element,
            quirks_mode,
            snapshot_table,
            invalidated: lightmount_ignore_relative_selector_invalidation::<E>,
            affected: Some(&collect_affected_root),
            sibling_traversal_map,
            _marker: std::marker::PhantomData,
        };
        invalidator.invalidate_relative_selectors_for_this(
            stylist,
            |candidate, scope, cascade_data, _quirks_mode, collector| {
                let mut dependencies = Vec::new();
                lightmount_collect_dependencies_from_invalidation_map(
                    cascade_data.relative_selector_invalidation_map(),
                    *candidate,
                    query,
                    &mut dependencies,
                );
                lightmount_collect_dependencies_from_additional_relative_invalidation_map(
                    cascade_data.relative_invalidation_map_attributes(),
                    query,
                    &mut dependencies,
                );
                matched_dependency_count += dependencies.len();
                for dependency in dependencies {
                    if lightmount_dependency_is_relative_selector(dependency) {
                        snapshot_relative_dependencies.push((scope, dependency));
                    }
                    collector.add_dependency(dependency, *candidate, scope);
                }
            },
        );
    }

    let snapshot_relative_roots = collect_snapshot_relative_roots(&snapshot_relative_dependencies);
    lightmount_relative_style_invalidation_query_result(
        affected_roots.into_inner(),
        &snapshot_relative_roots,
        matched_dependency_count,
        snapshot_relative_dependencies.len(),
    )
}

fn lightmount_ignore_relative_selector_invalidation<E>(_element: E, _result: &InvalidationResult) {}

impl LightmountStyleInvalidationQuery<'_> {
    fn drops_collected_relative_dependencies(&self) -> bool {
        matches!(self, Self::CustomState(_))
    }
}

impl<Root> LightmountSourceStyleInvalidationQueryResult<Root>
where
    Root: Eq + Hash,
{
    /// Merge two query results while preserving first-seen root and fallback
    /// reason order.
    #[inline]
    fn merged_with(self, other: Self) -> Self {
        let mut affected_roots = IndexSet::new();
        affected_roots.extend(self.affected_roots.into_iter().chain(other.affected_roots));

        let mut fallback_reasons = self.fallback_reasons;
        fallback_reasons.extend(other.fallback_reasons);

        Self {
            affected_roots: affected_roots.into_iter().collect(),
            empty_result_is_exact: self.empty_result_is_exact && other.empty_result_is_exact,
            matched_dependency_count: self.matched_dependency_count
                + other.matched_dependency_count,
            fallback_reasons,
        }
    }
}

/// Merge two source-local query results while preserving Stylo-owned fallback
/// and exactness semantics.
#[inline]
pub fn lightmount_merge_source_style_invalidation_query_results<Root>(
    existing: LightmountSourceStyleInvalidationQueryResult<Root>,
    incoming: LightmountSourceStyleInvalidationQueryResult<Root>,
) -> LightmountSourceStyleInvalidationQueryResult<Root>
where
    Root: Eq + Hash,
{
    existing.merged_with(incoming)
}

impl<Root> LightmountSourceStyleInvalidationResult<Root> {
    /// Construct a source-local invalidation result from already-classified
    /// parts.
    #[inline]
    fn from_parts(
        affected_roots: Vec<Root>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
    ) -> Self {
        Self {
            affected_roots,
            fallback_reasons,
            fallback_kind,
            fallback_root_availability,
            empty_result_is_exact,
            matched_dependency_count,
        }
    }

    /// Drain this result into the source-result policy owner.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountSourceStyleInvalidationResultSink<Root>) {
        target.set_source_style_invalidation_result(
            LightmountSourceStyleInvalidationResultParts::from_result(self),
        );
    }
}

/// Accumulates query-local affected roots and fallback reasons for one retained
/// stylesheet source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationResultAccumulator<Root: Eq + Hash> {
    affected_roots: Vec<Root>,
    affected_root_set: HashSet<Root>,
    fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
}

impl<Root> LightmountSourceStyleInvalidationResultAccumulator<Root>
where
    Root: Copy + Eq + Hash,
{
    /// Create an empty source-local result accumulator.
    #[inline]
    pub fn new() -> Self {
        Self {
            affected_roots: Vec::new(),
            affected_root_set: HashSet::new(),
            fallback_reasons: IndexSet::new(),
            empty_result_is_exact: true,
            matched_dependency_count: 0,
        }
    }

    /// Merge one retained dependency-query result into this source-local
    /// accumulator.
    #[inline]
    fn merge_query_result(
        &mut self,
        affected_roots: Vec<Root>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    ) {
        self.fallback_reasons.extend(fallback_reasons);
        self.empty_result_is_exact &= empty_result_is_exact;
        self.matched_dependency_count += matched_dependency_count;
        for root in affected_roots {
            lightmount_push_unique_root(
                &mut self.affected_roots,
                &mut self.affected_root_set,
                root,
            );
        }
    }

    /// Merge one typed retained dependency-query result into this source-local
    /// accumulator.
    #[inline]
    pub fn merge_invalidation_query_result(
        &mut self,
        result: LightmountSourceStyleInvalidationQueryResult<Root>,
    ) {
        let LightmountSourceStyleInvalidationQueryResult {
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
        } = result;
        self.merge_query_result(
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
        );
    }

    /// Convert the accumulated query results into the source-local result
    /// policy Lightmount should apply for this stylesheet source.
    #[inline]
    pub fn into_source_result(
        mut self,
        exact_safety_fallback_roots: &IndexSet<Root>,
    ) -> LightmountSourceStyleInvalidationResult<Root> {
        let needs_source_fallback =
            !self.fallback_reasons.is_empty() || self.has_inexact_empty_result();
        if needs_source_fallback && self.fallback_reasons.is_empty() {
            self.fallback_reasons
                .insert(LightmountSourceInvalidationFallbackReason::InexactEmptyResult);
        }
        if needs_source_fallback && exact_safety_fallback_roots.is_empty() {
            self.fallback_reasons
                .insert(LightmountSourceInvalidationFallbackReason::MissingFallbackRoots);
            return LightmountSourceStyleInvalidationResult::from_parts(
                self.affected_roots,
                self.fallback_reasons,
                Some(LightmountSourceStyleInvalidationSourceResultKind::MissingFallbackRoots),
                Some(LightmountSourceFallbackRootAvailability::Missing),
                self.empty_result_is_exact,
                self.matched_dependency_count,
            );
        }
        if needs_source_fallback {
            self.affected_roots.clear();
            self.affected_root_set.clear();
            for &root in exact_safety_fallback_roots {
                lightmount_push_unique_root(
                    &mut self.affected_roots,
                    &mut self.affected_root_set,
                    root,
                );
            }
        }
        LightmountSourceStyleInvalidationResult::from_parts(
            self.affected_roots,
            self.fallback_reasons,
            needs_source_fallback
                .then_some(LightmountSourceStyleInvalidationSourceResultKind::Fallback),
            if needs_source_fallback {
                LightmountSourceFallbackRootAvailability::for_root_count(
                    exact_safety_fallback_roots.len(),
                )
            } else {
                None
            },
            self.empty_result_is_exact,
            self.matched_dependency_count,
        )
    }

    fn has_inexact_empty_result(&self) -> bool {
        self.affected_roots.is_empty()
            && (self.matched_dependency_count == 0 || !self.empty_result_is_exact)
    }
}

impl<Root> Default for LightmountSourceStyleInvalidationResultAccumulator<Root>
where
    Root: Copy + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

fn lightmount_push_unique_root<Root: Copy + Eq + Hash>(
    roots: &mut Vec<Root>,
    root_set: &mut HashSet<Root>,
    root: Root,
) {
    if root_set.insert(root) {
        roots.push(root);
    }
}

/// Lightmount-facing retained invalidation result for a source-aware batch.
///
/// The source result table is the stored fact table. Runtime-specific cleanup
/// owners drain the table through sink traits instead of reading fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountInvalidationResult<Root> {
    source_results: Vec<LightmountSourceStyleInvalidationSourceResult<Root>>,
}

/// Builder for a Lightmount-facing retained invalidation source-result table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountInvalidationResultBuilder<Root> {
    source_results: Vec<LightmountSourceStyleInvalidationSourceResult<Root>>,
}

/// Sink used by a runtime owner to consume a Lightmount retained invalidation
/// source result table.
pub trait LightmountInvalidationSourceResultsSink<Root> {
    /// Record how many source-result rows will be drained.
    fn record_lightmount_invalidation_source_result_count(&mut self, count: usize);

    /// Record one retained source-result row.
    fn record_lightmount_invalidation_source_result(
        &mut self,
        result: LightmountSourceStyleInvalidationSourceResult<Root>,
    );
}

impl<Root> LightmountInvalidationResult<Root> {
    /// Create a result table from already classified source-result rows.
    #[inline]
    fn from_source_results(
        source_results: Vec<LightmountSourceStyleInvalidationSourceResult<Root>>,
    ) -> Self {
        Self { source_results }
    }

    /// Drain source-result rows into a runtime-owned sink.
    #[inline]
    pub fn drain_source_results_into(
        self,
        target: &mut impl LightmountInvalidationSourceResultsSink<Root>,
    ) {
        target.record_lightmount_invalidation_source_result_count(self.source_results.len());
        for result in self.source_results {
            target.record_lightmount_invalidation_source_result(result);
        }
    }
}

impl<Root> Default for LightmountInvalidationResult<Root> {
    fn default() -> Self {
        Self {
            source_results: Vec::new(),
        }
    }
}

impl<Root> Default for LightmountInvalidationResultBuilder<Root> {
    fn default() -> Self {
        Self {
            source_results: Vec::new(),
        }
    }
}

impl<Root> LightmountInvalidationResultBuilder<Root> {
    /// Create an empty source-result table builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push an already-classified source-result row.
    #[inline]
    fn push_source_result(&mut self, result: LightmountSourceStyleInvalidationSourceResult<Root>) {
        self.source_results.push(result);
    }

    /// Push an exact source-result row.
    #[inline]
    pub fn push_exact_source_result(
        &mut self,
        source_index: usize,
        affected_roots: Vec<Root>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
    ) {
        self.push_source_result(LightmountSourceStyleInvalidationSourceResult::exact_result(
            source_index,
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
        ));
    }

    /// Push a fallback source-result row.
    #[inline]
    pub fn push_fallback_source_result(
        &mut self,
        source_index: usize,
        kind: LightmountSourceStyleInvalidationSourceResultKind,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
        fallback_reasons: impl IntoIterator<Item = LightmountSourceInvalidationFallbackReason>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        affected_roots: Vec<Root>,
    ) {
        self.push_source_result(LightmountSourceStyleInvalidationSourceResult::fallback(
            source_index,
            kind,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons.into_iter().collect(),
            fallback_root_availability,
            affected_roots,
        ));
    }

    /// Finish and return the Lightmount-facing retained invalidation result.
    #[inline]
    pub fn finish(self) -> LightmountInvalidationResult<Root> {
        LightmountInvalidationResult::from_source_results(self.source_results)
    }
}

impl<Root> LightmountInvalidationResultBuilder<Root>
where
    Root: Copy + Eq + Hash,
{
    /// Push a fallback-only source-result row.
    #[inline]
    pub fn push_fallback_only_source(
        &mut self,
        source_index: usize,
        kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        fallback_roots: &IndexSet<Root>,
    ) {
        self.push_source_result(
            LightmountSourceStyleInvalidationSourceResult::fallback_only(
                source_index,
                kind,
                fallback_reasons,
                fallback_roots,
            ),
        );
    }

    /// Push a final source-result row from source-local query result policy and
    /// a planned fallback policy.
    #[inline]
    pub fn push_source_result_from_planned_fallback(
        &mut self,
        source_index: usize,
        source_result: LightmountSourceStyleInvalidationResult<Root>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: &IndexSet<Root>,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
    ) {
        self.push_source_result(
            LightmountSourceStyleInvalidationSourceResult::from_source_result_and_planned_fallback(
                source_index,
                source_result,
                fallback_kind,
                reasoned_fallback_roots,
                fallback_reasons,
            ),
        );
    }

    /// Push a retained source-result row for an unavailable retained system or
    /// source cascade.
    #[inline]
    pub fn push_unavailable_retained_source(
        &mut self,
        source_index: usize,
        reason: LightmountSourceInvalidationFallbackReason,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        reasoned_fallback_roots: &IndexSet<Root>,
        exact_safety_fallback_roots: &IndexSet<Root>,
    ) {
        self.push_source_result(
            LightmountSourceStyleInvalidationSourceResult::unavailable_retained_source(
                source_index,
                reason,
                fallback_reasons,
                reasoned_fallback_roots,
                exact_safety_fallback_roots,
            ),
        );
    }

    /// Push a source-result row for a missing retained style system.
    #[inline]
    pub fn push_missing_retained_style_system_source(
        &mut self,
        source_index: usize,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        reasoned_fallback_roots: &IndexSet<Root>,
        exact_safety_fallback_roots: &IndexSet<Root>,
    ) {
        self.push_unavailable_retained_source(
            source_index,
            LightmountSourceInvalidationFallbackReason::MissingRetainedStyleSystem,
            fallback_reasons,
            reasoned_fallback_roots,
            exact_safety_fallback_roots,
        );
    }

    /// Push a source-result row for missing retained source cascade data.
    #[inline]
    pub fn push_missing_retained_cascade_data_source(
        &mut self,
        source_index: usize,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        reasoned_fallback_roots: &IndexSet<Root>,
        exact_safety_fallback_roots: &IndexSet<Root>,
    ) {
        self.push_unavailable_retained_source(
            source_index,
            LightmountSourceInvalidationFallbackReason::MissingRetainedCascadeData,
            fallback_reasons,
            reasoned_fallback_roots,
            exact_safety_fallback_roots,
        );
    }
}

/// One source in a retained source invalidation batch and how it was resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationSourceResult<Root> {
    source_index: usize,
    kind: LightmountSourceStyleInvalidationSourceResultKind,
    exact: bool,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
    fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
    fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
    affected_roots: Vec<Root>,
}

/// Diagnostic facts for one retained source-result row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationTargetResultDiagnosticFacts {
    kind: LightmountSourceStyleInvalidationSourceResultKind,
    exact: bool,
    empty_result_is_exact: bool,
    matched_dependency_count: usize,
    fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
    fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
    affected_root_count: usize,
}

/// Cleanup facts for one retained source-result row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationTargetResultCleanupFacts {
    fallback_context_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
    clear_all_cleanup_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
    include_fallback_context_for_clear_all: bool,
    requires_fallback_handling: bool,
}

/// Cleanup and optional diagnostic facts for one retained source-result row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceStyleInvalidationTargetResultRecord {
    cleanup_facts: LightmountSourceStyleInvalidationTargetResultCleanupFacts,
    diagnostic_facts: Option<LightmountSourceStyleInvalidationTargetResultDiagnosticFacts>,
}

/// Drainable parts for one retained source-result row.
pub struct LightmountSourceStyleInvalidationSourceResultParts<Root> {
    source_index: usize,
    affected_roots: LightmountSourceAffectedRootsCleanup<Root>,
    target_result_record: LightmountSourceStyleInvalidationTargetResultRecord,
}

/// Affected roots classified for exact cleanup or source-fallback cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightmountSourceAffectedRootsCleanup<Root> {
    kind: LightmountSourceAffectedRootKind,
    roots: Vec<Root>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LightmountSourceAffectedRootKind {
    Exact,
    SourceFallback,
}

/// Sink used to drain diagnostic facts for one retained source-result row.
pub trait LightmountSourceStyleInvalidationTargetResultDiagnosticFactsSink {
    /// Set diagnostic facts for one retained source-result row.
    fn set_source_style_invalidation_target_result_diagnostic_facts(
        &mut self,
        facts: LightmountSourceStyleInvalidationTargetResultDiagnosticFacts,
    );
}

/// Sink used by diagnostic owners that consume target-result diagnostic fields.
pub trait LightmountSourceStyleInvalidationTargetResultDiagnosticFactsPartsSink {
    /// Set diagnostic fact fields for one retained source-result row.
    fn set_source_style_invalidation_target_result_diagnostic_fact_parts(
        &mut self,
        kind: LightmountSourceStyleInvalidationSourceResultKind,
        exact: bool,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
        fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        affected_root_count: usize,
    );
}

/// Sink used to drain cleanup facts for one retained source-result row.
pub trait LightmountSourceStyleInvalidationTargetResultCleanupFactsSink {
    /// Set cleanup facts for one retained source-result row.
    fn set_source_style_invalidation_target_result_cleanup_facts(
        &mut self,
        facts: LightmountSourceStyleInvalidationTargetResultCleanupFacts,
    );
}

/// Sink used by cleanup owners that consume target-result cleanup fields.
pub trait LightmountSourceStyleInvalidationTargetResultCleanupFactsPartsSink {
    /// Set cleanup fact fields for one retained source-result row.
    fn set_source_style_invalidation_target_result_cleanup_fact_parts(
        &mut self,
        fallback_context_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        clear_all_cleanup_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        include_fallback_context_for_clear_all: bool,
        requires_fallback_handling: bool,
    );
}

/// Sink used to drain affected roots from one retained source-result row.
pub trait LightmountSourceAffectedRootsCleanupSink<Root> {
    /// Extend exact affected roots.
    fn extend_exact_affected_roots(&mut self, roots: &[Root]);

    /// Extend source-fallback roots.
    fn extend_source_fallback_roots(&mut self, roots: &[Root]);
}

/// Sink used to consume retained source-result rows.
pub trait LightmountSourceStyleInvalidationSourceResultSink<Root> {
    /// Return whether diagnostic target-result facts should be retained.
    fn retain_source_style_invalidation_target_result_diagnostics(&self) -> bool {
        true
    }

    /// Record one retained source-result row artifact.
    fn record_source_style_invalidation_source_result(
        &mut self,
        parts: LightmountSourceStyleInvalidationSourceResultParts<Root>,
    );
}

/// Sink used by runtime owners that consume source-result row parts.
pub trait LightmountSourceStyleInvalidationSourceResultPartsSink<Root> {
    /// Record one retained source-result row's classified parts.
    fn record_source_style_invalidation_source_result_parts(
        &mut self,
        source_index: usize,
        affected_roots: LightmountSourceAffectedRootsCleanup<Root>,
        target_result_record: LightmountSourceStyleInvalidationTargetResultRecord,
    );
}

impl LightmountSourceStyleInvalidationTargetResultDiagnosticFacts {
    /// Drain this diagnostic row into a runtime-owned sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationTargetResultDiagnosticFactsSink,
    ) {
        target.set_source_style_invalidation_target_result_diagnostic_facts(self);
    }

    /// Drain this diagnostic row into a field-level sink.
    #[inline]
    pub fn drain_parts_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationTargetResultDiagnosticFactsPartsSink,
    ) {
        target.set_source_style_invalidation_target_result_diagnostic_fact_parts(
            self.kind,
            self.exact,
            self.empty_result_is_exact,
            self.matched_dependency_count,
            self.fallback_reasons,
            self.fallback_root_availability,
            self.affected_root_count,
        );
    }
}

impl LightmountSourceStyleInvalidationTargetResultCleanupFacts {
    /// Drain this cleanup row into a runtime-owned sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationTargetResultCleanupFactsSink,
    ) {
        target.set_source_style_invalidation_target_result_cleanup_facts(self);
    }

    /// Drain this cleanup row into a field-level sink.
    #[inline]
    pub fn drain_parts_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationTargetResultCleanupFactsPartsSink,
    ) {
        target.set_source_style_invalidation_target_result_cleanup_fact_parts(
            self.fallback_context_reasons,
            self.clear_all_cleanup_reasons,
            self.include_fallback_context_for_clear_all,
            self.requires_fallback_handling,
        );
    }
}

impl LightmountSourceStyleInvalidationTargetResultRecord {
    fn with_diagnostic_facts(
        diagnostic_facts: LightmountSourceStyleInvalidationTargetResultDiagnosticFacts,
        cleanup_facts: LightmountSourceStyleInvalidationTargetResultCleanupFacts,
    ) -> Self {
        Self {
            cleanup_facts,
            diagnostic_facts: Some(diagnostic_facts),
        }
    }

    fn cleanup_only(
        cleanup_facts: LightmountSourceStyleInvalidationTargetResultCleanupFacts,
    ) -> Self {
        Self {
            cleanup_facts,
            diagnostic_facts: None,
        }
    }

    /// Drain cleanup facts and return optional diagnostic facts.
    #[inline]
    pub fn drain_cleanup_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationTargetResultCleanupFactsSink,
    ) -> Option<LightmountSourceStyleInvalidationTargetResultDiagnosticFacts> {
        self.cleanup_facts.drain_into(target);
        self.diagnostic_facts
    }
}

impl<Root> LightmountSourceStyleInvalidationSourceResultParts<Root> {
    /// Drain this source-result row artifact into a parts sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationSourceResultPartsSink<Root>,
    ) {
        target.record_source_style_invalidation_source_result_parts(
            self.source_index,
            self.affected_roots,
            self.target_result_record,
        );
    }
}

impl<Root> LightmountSourceAffectedRootsCleanup<Root> {
    fn new(kind: LightmountSourceAffectedRootKind, roots: Vec<Root>) -> Self {
        Self { kind, roots }
    }

    /// Drain affected roots into a runtime-owned sink.
    #[inline]
    pub fn drain_into(self, target: &mut impl LightmountSourceAffectedRootsCleanupSink<Root>) {
        match self.kind {
            LightmountSourceAffectedRootKind::Exact => {
                target.extend_exact_affected_roots(&self.roots);
            },
            LightmountSourceAffectedRootKind::SourceFallback => {
                target.extend_source_fallback_roots(&self.roots);
            },
        }
    }
}

impl<Root> LightmountSourceStyleInvalidationSourceResult<Root>
where
    Root: Copy + Eq + Hash,
{
    /// Build a fallback-only source-result row.
    #[inline]
    fn fallback_only(
        source_index: usize,
        kind: LightmountRetainedSourceStyleInvalidationKind,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        fallback_roots: &IndexSet<Root>,
    ) -> Self {
        Self::fallback(
            source_index,
            kind.fallback_source_result_kind(!fallback_reasons.is_empty()),
            fallback_roots.is_empty(),
            0,
            fallback_reasons.iter().copied().collect(),
            kind.fallback_root_availability(fallback_roots.len()),
            fallback_roots.iter().copied().collect(),
        )
    }

    /// Build a source-result row for a retained source that could not be
    /// queried.
    #[inline]
    fn unavailable_retained_source(
        source_index: usize,
        reason: LightmountSourceInvalidationFallbackReason,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
        reasoned_fallback_roots: &IndexSet<Root>,
        exact_safety_fallback_roots: &IndexSet<Root>,
    ) -> Self {
        let kind = match reason {
            LightmountSourceInvalidationFallbackReason::MissingRetainedStyleSystem => {
                LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedStyleSystem
            },
            LightmountSourceInvalidationFallbackReason::MissingRetainedCascadeData => {
                LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedCascadeData
            },
            _ => LightmountSourceStyleInvalidationSourceResultKind::Fallback,
        };
        let mut reasons = fallback_reasons.iter().copied().collect::<Vec<_>>();
        if !fallback_reasons.contains(&reason) {
            reasons.push(reason);
        }
        let fallback_roots =
            lightmount_union_fallback_roots(reasoned_fallback_roots, exact_safety_fallback_roots);
        Self::fallback(
            source_index,
            kind,
            false,
            0,
            reasons,
            LightmountSourceFallbackRootAvailability::for_root_count(fallback_roots.len()),
            fallback_roots.iter().copied().collect(),
        )
    }

    /// Build a final source-result row from source-local query result policy
    /// and a planned fallback policy.
    #[inline]
    fn from_source_result_and_planned_fallback(
        source_index: usize,
        source_result: LightmountSourceStyleInvalidationResult<Root>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        reasoned_fallback_roots: &IndexSet<Root>,
        fallback_reasons: &IndexSet<LightmountSourceInvalidationFallbackReason>,
    ) -> Self {
        let LightmountSourceStyleInvalidationResult {
            mut affected_roots,
            fallback_reasons: source_fallback_reasons,
            fallback_kind: source_fallback_kind,
            fallback_root_availability: source_fallback_root_availability,
            empty_result_is_exact,
            matched_dependency_count,
        } = source_result;
        if !fallback_reasons.is_empty() {
            let mut affected_root_set = affected_roots.iter().copied().collect();
            for &root in reasoned_fallback_roots {
                lightmount_push_unique_root(&mut affected_roots, &mut affected_root_set, root);
            }
        }
        let mut merged_fallback_reasons = fallback_reasons.iter().copied().collect::<IndexSet<_>>();
        merged_fallback_reasons.extend(source_fallback_reasons);
        if merged_fallback_reasons.is_empty() {
            return Self::exact_result(
                source_index,
                affected_roots,
                empty_result_is_exact,
                matched_dependency_count,
            );
        }
        let kind = source_fallback_kind
            .or_else(|| fallback_kind.map(|kind| kind.fallback_source_result_kind(true)))
            .unwrap_or(LightmountSourceStyleInvalidationSourceResultKind::Fallback);
        let fallback_root_availability = source_fallback_root_availability.or_else(|| {
            fallback_kind
                .and_then(|kind| kind.fallback_root_availability(reasoned_fallback_roots.len()))
        });
        Self::fallback(
            source_index,
            kind,
            empty_result_is_exact,
            matched_dependency_count,
            merged_fallback_reasons.into_iter().collect(),
            fallback_root_availability,
            affected_roots,
        )
    }
}

impl<Root> LightmountSourceStyleInvalidationSourceResult<Root> {
    /// Build an exact source-result row.
    #[inline]
    fn exact_result(
        source_index: usize,
        affected_roots: Vec<Root>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
    ) -> Self {
        Self {
            source_index,
            kind: LightmountSourceStyleInvalidationSourceResultKind::Exact,
            exact: true,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons: Vec::new(),
            fallback_root_availability: None,
            affected_roots,
        }
    }

    /// Build a fallback source-result row.
    #[inline]
    fn fallback(
        source_index: usize,
        kind: LightmountSourceStyleInvalidationSourceResultKind,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
        fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        affected_roots: Vec<Root>,
    ) -> Self {
        debug_assert_ne!(
            kind,
            LightmountSourceStyleInvalidationSourceResultKind::Exact,
            "exact source results should use LightmountSourceStyleInvalidationSourceResult::exact_result"
        );
        Self {
            source_index,
            kind,
            exact: false,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
            fallback_root_availability,
            affected_roots,
        }
    }

    /// Drain this source-result row into a runtime-owned sink.
    #[inline]
    pub fn drain_into(
        self,
        target: &mut impl LightmountSourceStyleInvalidationSourceResultSink<Root>,
    ) {
        let retain_diagnostics =
            target.retain_source_style_invalidation_target_result_diagnostics();
        target.record_source_style_invalidation_source_result(
            self.into_source_result_cleanup_and_target_record_parts(retain_diagnostics),
        );
    }

    fn into_source_result_cleanup_and_target_record_parts(
        self,
        retain_diagnostics: bool,
    ) -> LightmountSourceStyleInvalidationSourceResultParts<Root> {
        let source_index = self.source_index;
        let affected_root_kind = self.affected_root_kind();
        let affected_root_count = self.affected_root_count();
        let clear_all_cleanup_reasons = self.clear_all_cleanup_reasons();
        let include_fallback_context_for_clear_all =
            lightmount_source_result_kind_includes_fallback_context_for_clear_all(self.kind);
        let requires_fallback_handling = self.requires_fallback_handling();
        let affected_roots =
            LightmountSourceAffectedRootsCleanup::new(affected_root_kind, self.affected_roots);
        let target_result_record = if retain_diagnostics {
            let fallback_context_reasons = self.fallback_reasons.clone();
            LightmountSourceStyleInvalidationTargetResultRecord::with_diagnostic_facts(
                LightmountSourceStyleInvalidationTargetResultDiagnosticFacts {
                    kind: self.kind,
                    exact: self.exact,
                    empty_result_is_exact: self.empty_result_is_exact,
                    matched_dependency_count: self.matched_dependency_count,
                    fallback_reasons: self.fallback_reasons,
                    fallback_root_availability: self.fallback_root_availability,
                    affected_root_count,
                },
                LightmountSourceStyleInvalidationTargetResultCleanupFacts {
                    fallback_context_reasons,
                    clear_all_cleanup_reasons,
                    include_fallback_context_for_clear_all,
                    requires_fallback_handling,
                },
            )
        } else {
            LightmountSourceStyleInvalidationTargetResultRecord::cleanup_only(
                LightmountSourceStyleInvalidationTargetResultCleanupFacts {
                    fallback_context_reasons: self.fallback_reasons,
                    clear_all_cleanup_reasons,
                    include_fallback_context_for_clear_all,
                    requires_fallback_handling,
                },
            )
        };
        LightmountSourceStyleInvalidationSourceResultParts {
            source_index,
            affected_roots,
            target_result_record,
        }
    }

    fn affected_root_count(&self) -> usize {
        self.affected_roots.len()
    }

    fn is_exact_source_result(&self) -> bool {
        self.exact
            && self.kind == LightmountSourceStyleInvalidationSourceResultKind::Exact
            && self.fallback_reasons.is_empty()
    }

    fn requires_fallback_handling(&self) -> bool {
        !self.is_exact_source_result()
    }

    fn has_inexact_empty_result(&self) -> bool {
        !(self.is_exact_source_result() && self.empty_result_is_exact)
    }

    fn has_fallback_clear_all_cleanup(&self) -> bool {
        matches!(
            self.fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Missing)
        ) || (!self.fallback_reasons.is_empty() && self.affected_roots.is_empty())
    }

    fn has_inexact_empty_clear_all_cleanup(&self) -> bool {
        self.affected_roots.is_empty() && self.has_inexact_empty_result()
    }

    fn clear_all_cleanup_reasons(&self) -> Vec<LightmountSourceInvalidationFallbackReason> {
        if self.has_fallback_clear_all_cleanup() {
            let mut reasons = self
                .fallback_reasons
                .iter()
                .copied()
                .collect::<IndexSet<_>>();
            if matches!(
                self.fallback_root_availability,
                Some(LightmountSourceFallbackRootAvailability::Missing)
            ) {
                reasons.insert(LightmountSourceInvalidationFallbackReason::MissingFallbackRoots);
            }
            return reasons.into_iter().collect();
        }
        if self.has_inexact_empty_clear_all_cleanup() {
            return vec![LightmountSourceInvalidationFallbackReason::InexactEmptyResult];
        }
        Vec::new()
    }

    fn affected_root_kind(&self) -> LightmountSourceAffectedRootKind {
        if self.is_exact_source_result() {
            return LightmountSourceAffectedRootKind::Exact;
        }
        LightmountSourceAffectedRootKind::SourceFallback
    }
}

fn lightmount_union_fallback_roots<Root: Copy + Eq + Hash>(
    fallback_roots: &IndexSet<Root>,
    exact_safety_fallback_roots: &IndexSet<Root>,
) -> IndexSet<Root> {
    let mut roots = fallback_roots.clone();
    roots.extend(exact_safety_fallback_roots.iter().copied());
    roots
}

fn lightmount_source_result_kind_includes_fallback_context_for_clear_all(
    kind: LightmountSourceStyleInvalidationSourceResultKind,
) -> bool {
    matches!(
        kind,
        LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedStyleSystem
            | LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedCascadeData
    )
}

/// Return the Lightmount fallback reason represented by a raw Stylo dependency
/// kind.
#[inline]
fn lightmount_dependency_fallback_reason_for_dependency(
    dependency: &Dependency,
) -> LightmountDependencyFallbackReason {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::FullSelector => {
            LightmountDependencyFallbackReason::FullSelector
        },
        DependencyInvalidationKind::Relative(_) => {
            LightmountDependencyFallbackReason::RelativeAnySelector
        },
        DependencyInvalidationKind::Scope(_) => LightmountDependencyFallbackReason::ScopeDependency,
        DependencyInvalidationKind::Normal(_) => {
            LightmountDependencyFallbackReason::UnsupportedDependency
        },
    }
}

/// Collect dependencies matching one Lightmount query from a Stylo invalidation
/// map.
#[inline]
pub fn lightmount_collect_dependencies_from_invalidation_map<'a, E>(
    map: &'a InvalidationMap,
    element: E,
    query: LightmountStyleInvalidationQuery<'_>,
    dependencies: &mut Vec<&'a Dependency>,
) where
    E: TElement,
{
    let quirks_mode = element.as_node().owner_doc().quirks_mode();
    match query {
        LightmountStyleInvalidationQuery::Universal => {
            dependencies.extend(map.any_to_selector.iter());
        },
        LightmountStyleInvalidationQuery::Type(local_name) => {
            if let Some(items) = map.type_to_selector.get(&LocalName::from(local_name)) {
                dependencies.extend(items.iter());
            }
        },
        LightmountStyleInvalidationQuery::Attribute(name) => {
            if let Some(items) = map
                .other_attribute_affecting_selectors
                .get(&LocalName::from(name))
            {
                dependencies.extend(items.iter());
            }
        },
        LightmountStyleInvalidationQuery::Class(token) => {
            if let Some(items) = map.class_to_selector.get(&Atom::from(token), quirks_mode) {
                dependencies.extend(items.iter());
            }
        },
        LightmountStyleInvalidationQuery::Id(value) => {
            if let Some(items) = map.id_to_selector.get(&Atom::from(value), quirks_mode) {
                dependencies.extend(items.iter());
            }
        },
        LightmountStyleInvalidationQuery::State(state) => {
            map.state_affecting_selectors.lookup_with_additional(
                element,
                quirks_mode,
                None,
                &[],
                state,
                |dependency| {
                    if dependency.state.intersects(state) {
                        dependencies.push(&dependency.dep);
                    }
                    true
                },
            );
        },
        LightmountStyleInvalidationQuery::CustomState(name) => {
            if let Some(items) = map
                .custom_state_affecting_selectors
                .get(&AtomIdent::from(name))
            {
                dependencies.extend(items.iter());
            }
        },
    }
}

/// Collect dependencies matching one Lightmount query from Stylo's additional
/// relative selector invalidation map.
#[inline]
pub fn lightmount_collect_dependencies_from_additional_relative_invalidation_map<'a>(
    map: &'a AdditionalRelativeSelectorInvalidationMap,
    query: LightmountStyleInvalidationQuery<'_>,
    dependencies: &mut Vec<&'a Dependency>,
) {
    if query == LightmountStyleInvalidationQuery::Universal {
        dependencies.extend(map.any_to_selector.iter());
    }
    if let LightmountStyleInvalidationQuery::Type(local_name) = query {
        if let Some(items) = map.type_to_selector.get(&LocalName::from(local_name)) {
            dependencies.extend(items.iter());
        }
    }
}

/// Return the Lightmount retained invalidation action represented by a raw
/// Stylo dependency.
#[inline]
pub fn lightmount_dependency_invalidation_action(
    dependency: &Dependency,
) -> LightmountDependencyInvalidationAction {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Element) => {
            LightmountDependencyInvalidationAction::Element
        },
        DependencyInvalidationKind::Normal(
            NormalDependencyInvalidationKind::ElementAndDescendants,
        ) => LightmountDependencyInvalidationAction::ElementAndDescendants,
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Descendants) => {
            LightmountDependencyInvalidationAction::Descendants
        },
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Siblings) => {
            LightmountDependencyInvalidationAction::Siblings
        },
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::SlottedElements) => {
            LightmountDependencyInvalidationAction::SlottedElements
        },
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Parts) => {
            LightmountDependencyInvalidationAction::Parts
        },
        DependencyInvalidationKind::Scope(scope_kind) => {
            LightmountDependencyInvalidationAction::Scope(
                lightmount_scope_dependency_invalidation_action(dependency, scope_kind),
            )
        },
        DependencyInvalidationKind::FullSelector | DependencyInvalidationKind::Relative(_) => {
            LightmountDependencyInvalidationAction::Fallback(
                LightmountSourceInvalidationFallbackReason::from(
                    lightmount_dependency_fallback_reason_for_dependency(dependency),
                ),
            )
        },
    }
}

/// Return the fallback reason for a Servo relative selector invalidation
/// callback that Lightmount cannot yet represent as exact affected roots.
#[inline]
pub fn lightmount_relative_selector_invalidation_fallback_reason(
    _kind: RelativeDependencyInvalidationKind,
    _dependency: &Dependency,
) -> LightmountSourceInvalidationFallbackReason {
    LightmountSourceInvalidationFallbackReason::RelativeAnySelector
}

/// Return the Lightmount candidate traversal action for a relative selector
/// dependency.
#[inline]
pub fn lightmount_relative_dependency_invalidation_action(
    dependency: &Dependency,
) -> Option<LightmountRelativeDependencyInvalidationAction> {
    let DependencyInvalidationKind::Relative(kind) = dependency.invalidation_kind() else {
        return None;
    };
    Some(lightmount_relative_dependency_action(kind))
}

/// Return whether this dependency is a relative selector dependency.
#[inline]
pub fn lightmount_dependency_is_relative_selector(dependency: &Dependency) -> bool {
    lightmount_relative_dependency_invalidation_action(dependency).is_some()
}

/// Return whether this dependency can be used as a snapshot-relative outer
/// dependency by Lightmount.
#[inline]
pub fn lightmount_snapshot_relative_outer_dependency_supported(dependency: &Dependency) -> bool {
    matches!(
        dependency.invalidation_kind(),
        DependencyInvalidationKind::Normal(
            NormalDependencyInvalidationKind::Element
                | NormalDependencyInvalidationKind::ElementAndDescendants
                | NormalDependencyInvalidationKind::Descendants
                | NormalDependencyInvalidationKind::Siblings
                | NormalDependencyInvalidationKind::SlottedElements
                | NormalDependencyInvalidationKind::Parts
        )
    )
}

fn lightmount_scope_dependency_invalidation_action(
    dependency: &Dependency,
    scope_kind: ScopeDependencyInvalidationKind,
) -> LightmountScopeDependencyInvalidationAction {
    if scope_kind == ScopeDependencyInvalidationKind::ImplicitScope {
        return LightmountScopeDependencyInvalidationAction::ImplicitScope;
    }
    if dependency.selector.is_rightmost(dependency.selector_offset) {
        let force_add = any_next_has_scope_in_negation(dependency);
        if scope_kind == ScopeDependencyInvalidationKind::ScopeEnd || force_add {
            return LightmountScopeDependencyInvalidationAction::ForceAtSubject { force_add };
        }
        return LightmountScopeDependencyInvalidationAction::CheckNextInScope;
    }
    LightmountScopeDependencyInvalidationAction::PushByCombinator
}

fn lightmount_relative_dependency_action(
    kind: RelativeDependencyInvalidationKind,
) -> LightmountRelativeDependencyInvalidationAction {
    match kind {
        RelativeDependencyInvalidationKind::Ancestors => {
            LightmountRelativeDependencyInvalidationAction::Ancestors
        },
        RelativeDependencyInvalidationKind::Parent => {
            LightmountRelativeDependencyInvalidationAction::Parent
        },
        RelativeDependencyInvalidationKind::PrevSibling => {
            LightmountRelativeDependencyInvalidationAction::PrevSibling
        },
        RelativeDependencyInvalidationKind::AncestorPrevSibling => {
            LightmountRelativeDependencyInvalidationAction::AncestorPrevSibling
        },
        RelativeDependencyInvalidationKind::EarlierSibling => {
            LightmountRelativeDependencyInvalidationAction::EarlierSibling
        },
        RelativeDependencyInvalidationKind::AncestorEarlierSibling => {
            LightmountRelativeDependencyInvalidationAction::AncestorEarlierSibling
        },
    }
}

/// Return whether Lightmount's retained invalidation processor can represent
/// this dependency without source fallback.
#[inline]
pub fn lightmount_dependency_supported_by_retained_processor(dependency: &Dependency) -> bool {
    if !matches!(
        dependency.invalidation_kind(),
        DependencyInvalidationKind::Normal(_) | DependencyInvalidationKind::Scope(_)
    ) {
        return false;
    }
    dependency.next.as_ref().is_none_or(|dependencies| {
        dependencies
            .as_ref()
            .slice()
            .iter()
            .all(lightmount_dependency_supported_by_retained_processor)
    })
}

/// Return whether an empty result for this dependency can be treated as an exact
/// no-op by Lightmount's retained invalidation processor.
#[inline]
pub fn lightmount_dependency_empty_result_supported_by_retained_processor(
    dependency: &Dependency,
) -> bool {
    if !matches!(
        dependency.invalidation_kind(),
        DependencyInvalidationKind::Scope(_)
            | DependencyInvalidationKind::Normal(
                NormalDependencyInvalidationKind::Element
                    | NormalDependencyInvalidationKind::ElementAndDescendants
                    | NormalDependencyInvalidationKind::Descendants
                    | NormalDependencyInvalidationKind::Siblings
                    | NormalDependencyInvalidationKind::SlottedElements
                    | NormalDependencyInvalidationKind::Parts
            )
    ) {
        return false;
    }
    dependency.next.as_ref().is_none_or(|dependencies| {
        dependencies
            .as_ref()
            .slice()
            .iter()
            .all(lightmount_dependency_empty_result_supported_by_retained_processor)
    })
}

/// Classify one raw dependency for Lightmount's retained invalidation
/// processor.
#[inline]
pub fn lightmount_retained_processor_dependency_effect(
    dependency: &Dependency,
) -> LightmountRetainedProcessorDependencyEffect {
    if !lightmount_dependency_supported_by_retained_processor(dependency) {
        return LightmountRetainedProcessorDependencyEffect::Fallback(
            LightmountSourceInvalidationFallbackReason::from(
                lightmount_dependency_fallback_reason_for_dependency(dependency),
            ),
        );
    }

    LightmountRetainedProcessorDependencyEffect::Retained {
        empty_result_is_exact: lightmount_dependency_empty_result_supported_by_retained_processor(
            dependency,
        ),
    }
}

/// Which fallback roots may be used when a dependency query is not exact.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LightmountDependencyFallbackRootPolicy {
    /// Mutation-context roots are sufficient as the conservative cleanup target.
    ContextRoots,
    /// The caller must use source-local or source-scope fallback roots.
    SourceFallback,
}

/// Source dependency fallback handling chosen from one dependency query result.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LightmountDependencyFallbackHandling {
    /// Mutation context roots can satisfy the fallback, when available.
    ContextRoots(IndexSet<LightmountSourceInvalidationFallbackReason>),
    /// The source's fallback roots are required.
    SourceFallback(IndexSet<LightmountSourceInvalidationFallbackReason>),
}

/// Dependency root categories needed by Lightmount's DOM-backed fallback-root
/// construction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LightmountDependencyContextRootFlags {
    /// The query cannot be covered by context roots and needs source fallback.
    requires_source_fallback: bool,
    /// The changed element's local subtree can be affected.
    local_subtree: bool,
    /// Ancestors of the changed element can be affected.
    ancestor_chain: bool,
    /// Following siblings of the changed element can be affected.
    following_siblings: bool,
    /// The query includes a direct previous-sibling relative dependency.
    direct_previous_sibling_relative: bool,
    /// The previous element sibling can be affected.
    previous_sibling: bool,
    /// Earlier element siblings can be affected.
    earlier_siblings: bool,
    /// Previous siblings of ancestors can be affected.
    ancestor_previous_siblings: bool,
    /// Earlier siblings of ancestors can be affected.
    ancestor_earlier_siblings: bool,
    /// Assigned elements matched by `::slotted(...)` can be affected.
    slotted_elements: bool,
    /// Exposed part elements matched by `::part(...)` can be affected.
    parts: bool,
}

/// Sink for DOM-backed context-root categories derived from one dependency
/// query result.
pub trait LightmountDependencyContextRootFlagsSink {
    /// Context roots are insufficient and source fallback is required.
    fn require_context_source_fallback(&mut self);

    /// Include the changed element's local subtree.
    fn include_context_local_subtree(&mut self);

    /// Include the changed element's ancestor chain.
    fn include_context_ancestor_chain(&mut self);

    /// Include following siblings from the changed element or next-sibling
    /// context.
    fn include_context_following_siblings(&mut self);

    /// Include following siblings of ancestors.
    fn include_context_ancestor_following_siblings(&mut self);

    /// Include the previous sibling from mutation context, or the changed root
    /// if no previous sibling is known.
    fn include_context_previous_sibling(&mut self);

    /// Include earlier siblings from mutation context, or from the changed root
    /// if no previous sibling is known.
    fn include_context_earlier_siblings(&mut self);

    /// Include previous siblings of the changed root's ancestors.
    fn include_context_ancestor_previous_siblings(&mut self);

    /// Include assigned/slotted elements.
    fn include_context_slotted_elements(&mut self);

    /// Include exposed part roots.
    fn include_context_parts(&mut self);
}

/// Sink that can materialize DOM-backed context roots after Stylo has drained
/// the dependency root categories.
pub trait LightmountDependencyInvalidationContextRootsSink<Root>:
    LightmountDependencyContextRootFlagsSink + Sized
{
    /// Drain collected context roots into a Stylo-owned typed roots builder.
    fn drain_collected_context_roots_into(
        self,
        target: &mut impl LightmountDependencyInvalidationContextRootsPartsSink<Root>,
    );
}

/// Sink for the final DOM-backed context roots collected by an adapter.
pub trait LightmountDependencyInvalidationContextRootsPartsSink<Root> {
    /// Context roots are insufficient and source fallback is required.
    fn record_context_source_fallback(&mut self);

    /// Extend the typed context-root result with collected DOM roots.
    fn extend_context_roots(&mut self, roots: Vec<Root>);
}

struct LightmountDependencyInvalidationContextRootsBuilder<Root> {
    requires_source_fallback: bool,
    roots: Vec<Root>,
}

impl<Root> LightmountDependencyInvalidationContextRootsBuilder<Root> {
    #[inline]
    fn finish(self) -> LightmountDependencyInvalidationContextRoots<Root> {
        LightmountDependencyInvalidationContextRoots::new(self.requires_source_fallback, self.roots)
    }
}

impl<Root> Default for LightmountDependencyInvalidationContextRootsBuilder<Root> {
    #[inline]
    fn default() -> Self {
        Self {
            requires_source_fallback: false,
            roots: Vec::new(),
        }
    }
}

impl<Root> LightmountDependencyInvalidationContextRootsPartsSink<Root>
    for LightmountDependencyInvalidationContextRootsBuilder<Root>
{
    #[inline]
    fn record_context_source_fallback(&mut self) {
        self.requires_source_fallback = true;
    }

    #[inline]
    fn extend_context_roots(&mut self, roots: Vec<Root>) {
        self.roots.extend(roots);
    }
}

impl LightmountDependencyContextRootFlags {
    /// Drain these context-root categories into a DOM-backed sink.
    #[inline]
    fn drain_into(
        self,
        allow_direct_previous_following_sibling_fallback: bool,
        target: &mut impl LightmountDependencyContextRootFlagsSink,
    ) {
        if self.requires_source_fallback {
            target.require_context_source_fallback();
        }
        if self.local_subtree {
            target.include_context_local_subtree();
        }
        if self.ancestor_chain {
            target.include_context_ancestor_chain();
        }
        if self.includes_following_siblings(allow_direct_previous_following_sibling_fallback) {
            if self.ancestor_chain {
                target.include_context_ancestor_following_siblings();
            } else {
                target.include_context_following_siblings();
            }
        }
        if self.previous_sibling {
            target.include_context_previous_sibling();
        }
        if self.earlier_siblings {
            target.include_context_earlier_siblings();
        }
        if self.ancestor_previous_siblings || self.ancestor_earlier_siblings {
            target.include_context_ancestor_previous_siblings();
        }
        if self.slotted_elements {
            target.include_context_slotted_elements();
        }
        if self.parts {
            target.include_context_parts();
        }
    }

    #[inline]
    fn includes_following_siblings(
        self,
        allow_direct_previous_following_sibling_fallback: bool,
    ) -> bool {
        self.following_siblings
            && !(!allow_direct_previous_following_sibling_fallback
                && self.direct_previous_sibling_relative
                && !self.earlier_siblings
                && !self.ancestor_previous_siblings
                && !self.ancestor_earlier_siblings)
    }
}

impl LightmountDependencyContextRootPlan {
    #[inline]
    fn new(
        query: &LightmountDependencyQueryResult,
        allow_direct_previous_following_sibling_fallback: bool,
    ) -> Self {
        Self {
            flags: query.context_root_flags(),
            allow_direct_previous_following_sibling_fallback,
        }
    }

    /// Drain this plan into a DOM-backed root sink.
    #[inline]
    pub fn drain_into<Root, Sink>(
        self,
        mut sink: Sink,
    ) -> LightmountDependencyInvalidationContextRoots<Root>
    where
        Sink: LightmountDependencyInvalidationContextRootsSink<Root>,
    {
        self.flags.drain_into(
            self.allow_direct_previous_following_sibling_fallback,
            &mut sink,
        );
        let mut builder = LightmountDependencyInvalidationContextRootsBuilder::default();
        sink.drain_collected_context_roots_into(&mut builder);
        builder.finish()
    }
}

/// Build typed dependency context roots by draining one dependency query's
/// root-category plan into an adapter-provided DOM sink.
#[cfg(test)]
fn lightmount_dependency_invalidation_context_roots<Root, Sink>(
    query: &LightmountDependencyQueryResult,
    allow_direct_previous_following_sibling_fallback: bool,
    sink: Sink,
) -> LightmountDependencyInvalidationContextRoots<Root>
where
    Sink: LightmountDependencyInvalidationContextRootsSink<Root>,
{
    LightmountDependencyContextRootPlan::new(
        query,
        allow_direct_previous_following_sibling_fallback,
    )
    .drain_into(sink)
}

/// Conservative query result for one changed class/id/attribute/state token.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyQueryResult {
    kinds: Vec<LightmountDependencyKind>,
    unknown_dependency: bool,
    fallback_reasons: Vec<LightmountDependencyFallbackReason>,
}

impl LightmountDependencyQueryResult {
    fn add_kind(&mut self, kind: LightmountDependencyKind) {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
    }

    fn add_fallback_reason(&mut self, reason: LightmountDependencyFallbackReason) {
        if !self.fallback_reasons.contains(&reason) {
            self.fallback_reasons.push(reason);
        }
        if matches!(
            reason,
            LightmountDependencyFallbackReason::UnknownDependency
        ) {
            self.unknown_dependency = true;
        }
    }

    fn extend(&mut self, other: Self) {
        for kind in other.kinds {
            self.add_kind(kind);
        }
        self.unknown_dependency |= other.unknown_dependency;
        for reason in other.fallback_reasons {
            self.add_fallback_reason(reason);
        }
        if other.unknown_dependency {
            self.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        }
    }

    /// Returns whether any dependency matched the query.
    #[inline]
    pub fn has_any_dependency(&self) -> bool {
        self.unknown_dependency || !self.kinds.is_empty() || !self.fallback_reasons.is_empty()
    }

    /// Returns whether this query can invalidate descendants of the changed
    /// element.
    #[inline]
    pub fn has_descendants_dependency(&self) -> bool {
        self.kinds.contains(&LightmountDependencyKind::Descendants)
    }

    /// Returns whether this query can invalidate `::slotted(...)` elements.
    #[inline]
    pub fn has_slotted_elements_dependency(&self) -> bool {
        self.kinds
            .contains(&LightmountDependencyKind::SlottedElements)
    }

    /// Returns whether this query can invalidate `::part(...)` elements.
    #[inline]
    pub fn has_parts_dependency(&self) -> bool {
        self.kinds.contains(&LightmountDependencyKind::Parts)
    }

    /// Returns whether this query can invalidate relative selector ancestor
    /// anchors.
    #[inline]
    pub fn has_relative_ancestors_dependency(&self) -> bool {
        self.kinds
            .contains(&LightmountDependencyKind::RelativeAncestors)
    }

    /// Returns the concrete dependency kinds captured for this query.
    #[inline]
    #[cfg(test)]
    fn kinds(&self) -> &[LightmountDependencyKind] {
        &self.kinds
    }

    /// Returns conservative fallback reasons captured for this query.
    #[inline]
    #[cfg(test)]
    fn fallback_reasons(&self) -> &[LightmountDependencyFallbackReason] {
        &self.fallback_reasons
    }

    /// Returns whether this query requires conservative fallback handling.
    #[inline]
    pub fn requires_fallback(&self) -> bool {
        !self.fallback_reasons.is_empty()
    }

    /// Returns the fallback-root policy for this dependency query.
    #[inline]
    fn fallback_root_policy(&self) -> LightmountDependencyFallbackRootPolicy {
        if !self.fallback_reasons.is_empty()
            && self.fallback_reasons.iter().all(|reason| {
                matches!(
                    reason,
                    LightmountDependencyFallbackReason::NestedRelativeSelectorDependency
                        | LightmountDependencyFallbackReason::NthOfDependency
                )
            })
        {
            LightmountDependencyFallbackRootPolicy::ContextRoots
        } else {
            LightmountDependencyFallbackRootPolicy::SourceFallback
        }
    }

    /// Returns explicit fallback reasons, or conservative shape-derived reasons
    /// when the caller has already determined this query needs fallback handling.
    #[inline]
    fn fallback_or_shape_reasons(&self) -> Vec<LightmountDependencyFallbackReason> {
        if !self.fallback_reasons.is_empty() {
            return self.fallback_reasons.clone();
        }
        if self.kinds.contains(&LightmountDependencyKind::Scope) {
            return vec![LightmountDependencyFallbackReason::ScopeDependency];
        }
        vec![LightmountDependencyFallbackReason::UnsupportedDependency]
    }

    /// Returns source invalidation fallback reasons for this query result.
    #[inline]
    fn source_invalidation_fallback_reasons(
        &self,
    ) -> IndexSet<LightmountSourceInvalidationFallbackReason> {
        self.fallback_or_shape_reasons()
            .into_iter()
            .map(LightmountSourceInvalidationFallbackReason::from)
            .collect()
    }

    /// Returns source dependency fallback handling for this query result.
    #[inline]
    fn source_dependency_fallback_handling(&self) -> LightmountDependencyFallbackHandling {
        let reasons = self.source_invalidation_fallback_reasons();
        match self.fallback_root_policy() {
            LightmountDependencyFallbackRootPolicy::ContextRoots => {
                LightmountDependencyFallbackHandling::ContextRoots(reasons)
            },
            LightmountDependencyFallbackRootPolicy::SourceFallback => {
                LightmountDependencyFallbackHandling::SourceFallback(reasons)
            },
        }
    }

    /// Returns whether the query can affect following sibling invalidation.
    #[inline]
    pub fn has_sibling_dependency(&self) -> bool {
        self.unknown_dependency
            || self.kinds.iter().any(|kind| {
                matches!(
                    kind,
                    LightmountDependencyKind::Siblings
                        | LightmountDependencyKind::RelativePrevSibling
                        | LightmountDependencyKind::RelativeAncestorPrevSibling
                        | LightmountDependencyKind::RelativeEarlierSibling
                        | LightmountDependencyKind::RelativeAncestorEarlierSibling
                )
            })
    }

    /// Returns whether this query can affect relative selector anchors.
    #[inline]
    fn has_relative_selector_dependency(&self) -> bool {
        self.kinds.iter().any(|kind| {
            matches!(
                kind,
                LightmountDependencyKind::RelativeAncestors
                    | LightmountDependencyKind::RelativeParent
                    | LightmountDependencyKind::RelativePrevSibling
                    | LightmountDependencyKind::RelativeEarlierSibling
                    | LightmountDependencyKind::RelativeAncestorPrevSibling
                    | LightmountDependencyKind::RelativeAncestorEarlierSibling
            )
        }) || self.fallback_reasons.iter().any(|reason| {
            matches!(
                reason,
                LightmountDependencyFallbackReason::RelativeAnySelector
                    | LightmountDependencyFallbackReason::NestedRelativeSelectorDependency
            )
        })
    }

    /// Returns whether this query can affect previous-sibling relative selector
    /// anchors.
    #[inline]
    fn has_relative_previous_sibling_dependency(&self) -> bool {
        self.kinds.iter().any(|kind| {
            matches!(
                kind,
                LightmountDependencyKind::RelativePrevSibling
                    | LightmountDependencyKind::RelativeEarlierSibling
                    | LightmountDependencyKind::RelativeAncestorPrevSibling
                    | LightmountDependencyKind::RelativeAncestorEarlierSibling
            )
        })
    }

    /// Returns whether this query is limited to a direct previous-sibling
    /// relative dependency plus the following-sibling dependency Stylo records
    /// for `+`.
    #[inline]
    fn has_only_direct_relative_previous_sibling_dependency(&self) -> bool {
        !self.kinds.is_empty()
            && self.kinds.iter().all(|kind| {
                matches!(
                    kind,
                    LightmountDependencyKind::RelativePrevSibling
                        | LightmountDependencyKind::Siblings
                )
            })
            && self
                .kinds
                .contains(&LightmountDependencyKind::RelativePrevSibling)
    }

    /// Returns whether this dependency needs structural context fallback
    /// cleanup for a universal child-list structural request.
    #[inline]
    fn requires_structural_context_fallback_cleanup(
        &self,
        request_requires_child_list_structural_dependency: bool,
        query_is_universal: bool,
    ) -> bool {
        request_requires_child_list_structural_dependency
            && query_is_universal
            && self.has_relative_selector_dependency()
    }

    /// Returns whether this query can affect `::slotted(...)` invalidation.
    #[inline]
    fn has_slotted_dependency(&self) -> bool {
        self.unknown_dependency
            || self
                .kinds
                .iter()
                .any(|kind| matches!(kind, LightmountDependencyKind::SlottedElements))
    }

    /// Returns the fallback-root categories this query can affect.
    #[inline]
    fn context_root_flags(&self) -> LightmountDependencyContextRootFlags {
        let mut flags = LightmountDependencyContextRootFlags {
            requires_source_fallback: self.requires_fallback(),
            ..LightmountDependencyContextRootFlags::default()
        };
        for kind in &self.kinds {
            match kind {
                LightmountDependencyKind::Element
                | LightmountDependencyKind::ElementAndDescendants
                | LightmountDependencyKind::Descendants => {
                    flags.local_subtree = true;
                },
                LightmountDependencyKind::Siblings => {
                    flags.following_siblings = true;
                },
                LightmountDependencyKind::SlottedElements => {
                    flags.slotted_elements = true;
                },
                LightmountDependencyKind::Parts => {
                    flags.parts = true;
                },
                LightmountDependencyKind::RelativeAncestors
                | LightmountDependencyKind::RelativeParent => {
                    flags.ancestor_chain = true;
                },
                LightmountDependencyKind::RelativePrevSibling => {
                    flags.direct_previous_sibling_relative = true;
                    flags.previous_sibling = true;
                },
                LightmountDependencyKind::RelativeEarlierSibling => {
                    flags.earlier_siblings = true;
                },
                LightmountDependencyKind::RelativeAncestorPrevSibling => {
                    flags.ancestor_chain = true;
                    flags.ancestor_previous_siblings = true;
                },
                LightmountDependencyKind::RelativeAncestorEarlierSibling => {
                    flags.ancestor_chain = true;
                    flags.ancestor_earlier_siblings = true;
                },
                LightmountDependencyKind::Scope => {
                    // Source-batch planning only has a coarse dependency
                    // summary. Scope dependencies need the retained scope
                    // action walker with selector-offset and next-dependency
                    // context, so mutation context roots are not sufficient.
                    flags.requires_source_fallback = true;
                },
            }
        }
        flags
    }
}

/// Keyed dependency query summary exposed for Lightmount.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyInvalidationSummary {
    class_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    id_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    type_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
    universal_dependency: LightmountDependencyQueryResult,
    attribute_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
    custom_state_dependencies: Vec<(AtomIdent, LightmountDependencyQueryResult)>,
    state_dependencies: Vec<(u64, LightmountDependencyQueryResult)>,
    unknown_state_dependency_bits: u64,
    focus_dependency: LightmountDependencyQueryResult,
    focus_within_dependency: LightmountDependencyQueryResult,
    target_dependency: LightmountDependencyQueryResult,
    unknown_dependency: bool,
}

impl LightmountDependencyInvalidationSummary {
    fn note_class_dependency(&mut self, class: Atom, result: LightmountDependencyQueryResult) {
        lightmount_note_keyed_dependency(&mut self.class_dependencies, class, result);
    }

    fn note_id_dependency(&mut self, id: Atom, result: LightmountDependencyQueryResult) {
        lightmount_note_keyed_dependency(&mut self.id_dependencies, id, result);
    }

    fn note_attribute_dependency(
        &mut self,
        attribute: LocalName,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.attribute_dependencies, attribute, result);
    }

    fn note_type_dependency(
        &mut self,
        local_name: LocalName,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.type_dependencies, local_name, result);
    }

    fn note_universal_dependency(&mut self, result: LightmountDependencyQueryResult) {
        self.universal_dependency.extend(result);
    }

    fn note_state_dependency(
        &mut self,
        state: ElementState,
        result: LightmountDependencyQueryResult,
    ) {
        if state.is_empty() {
            self.unknown_dependency = true;
            return;
        }
        lightmount_note_keyed_dependency(
            &mut self.state_dependencies,
            state.bits(),
            result.clone(),
        );
        if state.intersects(ElementState::FOCUS | ElementState::FOCUSRING) {
            self.focus_dependency.extend(result.clone());
        }
        if state.intersects(ElementState::FOCUS_WITHIN) {
            self.focus_dependency.extend(result.clone());
            self.focus_within_dependency.extend(result.clone());
        }
        if state.intersects(ElementState::URLTARGET) {
            self.target_dependency.extend(result);
        }
    }

    fn note_custom_state_dependency(
        &mut self,
        state: AtomIdent,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.custom_state_dependencies, state, result);
    }

    pub(crate) fn note_nth_of_class_dependency(&mut self, class: Atom) {
        self.note_class_dependency(class, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_id_dependency(&mut self, id: Atom) {
        self.note_id_dependency(id, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_attribute_dependency(&mut self, attribute: LocalName) {
        self.note_attribute_dependency(attribute, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_state_dependency(&mut self, state: ElementState) {
        if state.is_empty() {
            return;
        }
        self.note_state_dependency(state, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_unrepresented_state_dependencies(&mut self, state: ElementState) {
        if state.is_empty() {
            return;
        }
        let represented_bits = self
            .state_dependencies
            .iter()
            .fold(0, |bits, (state_bits, _)| bits | *state_bits);
        self.unknown_state_dependency_bits |= state.bits() & !represented_bits;
    }

    fn mark_unknown_dependency(&mut self) {
        self.unknown_dependency = true;
    }

    pub(crate) fn extend(&mut self, other: Self) {
        for (class, result) in other.class_dependencies {
            self.note_class_dependency(class, result);
        }
        for (id, result) in other.id_dependencies {
            self.note_id_dependency(id, result);
        }
        for (attribute, result) in other.attribute_dependencies {
            self.note_attribute_dependency(attribute, result);
        }
        for (local_name, result) in other.type_dependencies {
            self.note_type_dependency(local_name, result);
        }
        for (state, result) in other.custom_state_dependencies {
            self.note_custom_state_dependency(state, result);
        }
        self.universal_dependency.extend(other.universal_dependency);
        for (state_bits, result) in other.state_dependencies {
            lightmount_note_keyed_dependency(&mut self.state_dependencies, state_bits, result);
        }
        self.unknown_state_dependency_bits |= other.unknown_state_dependency_bits;
        self.focus_dependency.extend(other.focus_dependency);
        self.focus_within_dependency
            .extend(other.focus_within_dependency);
        self.target_dependency.extend(other.target_dependency);
        self.unknown_dependency |= other.unknown_dependency;
    }

    /// Query dependencies for a changed class token.
    pub fn query_class(&self, class: &Atom) -> LightmountDependencyQueryResult {
        self.class_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == class).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for a changed id.
    pub fn query_id(&self, id: &Atom) -> LightmountDependencyQueryResult {
        self.id_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == id).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for a changed attribute.
    pub fn query_attribute(&self, attribute: &LocalName) -> LightmountDependencyQueryResult {
        self.attribute_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == attribute).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for an inserted or removed element local name.
    pub fn query_type(&self, local_name: &LocalName) -> LightmountDependencyQueryResult {
        self.type_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == local_name).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for an inserted or removed element matching `*`.
    pub fn query_universal(&self) -> LightmountDependencyQueryResult {
        self.universal_dependency.clone()
    }

    /// Query dependencies for a changed element state bitset.
    pub fn query_state(&self, state: ElementState) -> LightmountDependencyQueryResult {
        let mut result = LightmountDependencyQueryResult::default();
        let bits = state.bits();
        for (candidate_bits, candidate_result) in &self.state_dependencies {
            if candidate_bits & bits != 0 {
                result.extend(candidate_result.clone());
            }
        }
        if self.unknown_state_dependency_bits & bits != 0 {
            result.add_fallback_reason(
                LightmountDependencyFallbackReason::UnsupportedStateDependency,
            );
        }
        result
    }

    /// Query dependencies for a changed CSS custom state.
    pub fn query_custom_state(&self, state: &AtomIdent) -> LightmountDependencyQueryResult {
        self.custom_state_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == state).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for focus-like state changes.
    pub fn query_focus(&self) -> LightmountDependencyQueryResult {
        self.focus_dependency.clone()
    }

    /// Query dependencies for :focus-within state changes.
    pub fn query_focus_within(&self) -> LightmountDependencyQueryResult {
        self.focus_within_dependency.clone()
    }

    /// Query dependencies for :target state changes.
    pub fn query_target(&self) -> LightmountDependencyQueryResult {
        self.target_dependency.clone()
    }

    /// Returns whether any unkeyed dependency was found.
    #[inline]
    pub fn has_unknown_dependency(&self) -> bool {
        self.unknown_dependency
    }

    /// Returns whether any known dependency can affect following siblings.
    #[inline]
    pub fn has_sibling_dependency(&self) -> bool {
        self.unknown_dependency
            || self.unknown_state_dependency_bits != 0
            || self
                .class_dependencies
                .iter()
                .chain(self.id_dependencies.iter())
                .map(|(_, result)| result)
                .chain(self.attribute_dependencies.iter().map(|(_, result)| result))
                .chain(
                    self.custom_state_dependencies
                        .iter()
                        .map(|(_, result)| result),
                )
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
                .chain(std::iter::once(&self.universal_dependency))
                .any(LightmountDependencyQueryResult::has_sibling_dependency)
            || self
                .type_dependencies
                .iter()
                .map(|(_, result)| result)
                .any(LightmountDependencyQueryResult::has_sibling_dependency)
    }

    /// Returns whether any known dependency can affect relative selector
    /// anchors.
    #[inline]
    pub fn has_relative_selector_dependency(&self) -> bool {
        self.class_dependencies
            .iter()
            .chain(self.id_dependencies.iter())
            .map(|(_, result)| result)
            .chain(self.attribute_dependencies.iter().map(|(_, result)| result))
            .chain(
                self.custom_state_dependencies
                    .iter()
                    .map(|(_, result)| result),
            )
            .chain(self.state_dependencies.iter().map(|(_, result)| result))
            .chain(std::iter::once(&self.focus_dependency))
            .chain(std::iter::once(&self.focus_within_dependency))
            .chain(std::iter::once(&self.target_dependency))
            .chain(std::iter::once(&self.universal_dependency))
            .any(LightmountDependencyQueryResult::has_relative_selector_dependency)
            || self
                .type_dependencies
                .iter()
                .map(|(_, result)| result)
                .any(LightmountDependencyQueryResult::has_relative_selector_dependency)
    }

    /// Returns whether any known dependency can affect `::slotted(...)`
    /// invalidation.
    #[inline]
    pub fn has_slotted_dependency(&self) -> bool {
        self.unknown_dependency
            || self.unknown_state_dependency_bits != 0
            || self
                .class_dependencies
                .iter()
                .chain(self.id_dependencies.iter())
                .map(|(_, result)| result)
                .chain(self.attribute_dependencies.iter().map(|(_, result)| result))
                .chain(
                    self.custom_state_dependencies
                        .iter()
                        .map(|(_, result)| result),
                )
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
                .chain(std::iter::once(&self.universal_dependency))
                .any(LightmountDependencyQueryResult::has_slotted_dependency)
    }
}

fn lightmount_nth_of_dependency_query_result() -> LightmountDependencyQueryResult {
    let mut result = LightmountDependencyQueryResult::default();
    result.add_kind(LightmountDependencyKind::Siblings);
    result.add_fallback_reason(LightmountDependencyFallbackReason::NthOfDependency);
    result
}

fn lightmount_note_keyed_dependency<K: Eq>(
    dependencies: &mut Vec<(K, LightmountDependencyQueryResult)>,
    key: K,
    result: LightmountDependencyQueryResult,
) {
    if !result.has_any_dependency() {
        return;
    }
    if let Some((_, existing)) = dependencies
        .iter_mut()
        .find(|(candidate, _)| candidate == &key)
    {
        existing.extend(result);
        return;
    }
    dependencies.push((key, result));
}

pub(crate) fn lightmount_sibling_summary_for_invalidation_map(
    map: &InvalidationMap,
) -> LightmountSiblingInvalidationSummary {
    let mut summary = LightmountSiblingInvalidationSummary::default();
    if map.unkeyed_sibling_dependency {
        summary.note_unknown_dependency();
    }
    if lightmount_dependencies_have_sibling_sensitive(&map.any_to_selector) {
        summary.note_unknown_dependency();
    }
    for (class, dependencies) in map.class_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.class_dependencies.push(class.clone());
        }
    }
    for (id, dependencies) in map.id_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.id_dependencies.push(id.clone());
        }
    }
    for (attribute, dependencies) in map.other_attribute_affecting_selectors.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.attribute_dependencies.push(attribute.clone());
        }
    }
    for (_, dependencies) in map.custom_state_affecting_selectors.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.note_unknown_dependency();
        }
    }
    lightmount_collect_state_sibling_dependencies_from_selector_map(
        &map.state_affecting_selectors,
        &mut summary,
    );
    summary
}

pub(crate) fn lightmount_sibling_summary_for_relative_invalidation_map(
    map: &AdditionalRelativeSelectorInvalidationMap,
) -> LightmountSiblingInvalidationSummary {
    let mut summary = LightmountSiblingInvalidationSummary::default();
    if map.needs_ancestors_traversal {
        summary.note_unknown_dependency();
    }
    for dependency in &map.any_to_selector {
        if lightmount_dependency_is_sibling_sensitive(dependency) {
            summary.note_unknown_dependency();
        }
    }
    for (_, dependencies) in map.type_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.note_unknown_dependency();
        }
    }
    if lightmount_selector_map_has_sibling_dependency(&map.ts_state_to_selector, |dependency| {
        &dependency.dep
    }) {
        summary.note_unknown_dependency();
    }
    summary
}

pub(crate) fn lightmount_dependency_summary_for_invalidation_map(
    map: &InvalidationMap,
) -> LightmountDependencyInvalidationSummary {
    let mut summary = LightmountDependencyInvalidationSummary::default();
    if map.unkeyed_sibling_dependency {
        summary.mark_unknown_dependency();
    }
    summary.note_universal_dependency(lightmount_dependency_query_result_for_dependencies(
        &map.any_to_selector,
    ));
    for (class, dependencies) in map.class_to_selector.iter() {
        summary.note_class_dependency(
            class.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (id, dependencies) in map.id_to_selector.iter() {
        summary.note_id_dependency(
            id.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (attribute, dependencies) in map.other_attribute_affecting_selectors.iter() {
        summary.note_attribute_dependency(
            attribute.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (local_name, dependencies) in map.type_to_selector.iter() {
        summary.note_type_dependency(
            local_name.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (state, dependencies) in map.custom_state_affecting_selectors.iter() {
        summary.note_custom_state_dependency(
            state.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    lightmount_collect_state_dependencies_from_selector_map(
        &map.state_affecting_selectors,
        &mut summary,
    );
    summary
}

pub(crate) fn lightmount_dependency_summary_for_relative_invalidation_map(
    map: &AdditionalRelativeSelectorInvalidationMap,
) -> LightmountDependencyInvalidationSummary {
    let mut summary = LightmountDependencyInvalidationSummary::default();
    if map.needs_ancestors_traversal {
        summary.mark_unknown_dependency();
    }
    summary.note_universal_dependency(lightmount_dependency_query_result_for_dependencies(
        &map.any_to_selector,
    ));
    for (local_name, dependencies) in map.type_to_selector.iter() {
        summary.note_type_dependency(
            local_name.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    if !map.ts_state_to_selector.is_empty() {
        summary.mark_unknown_dependency();
    }
    summary
}

fn lightmount_dependency_query_result_for_dependencies(
    dependencies: &[Dependency],
) -> LightmountDependencyQueryResult {
    let mut result = LightmountDependencyQueryResult::default();
    for dependency in dependencies {
        lightmount_collect_dependency_query_result(dependency, &mut result);
    }
    result
}

fn lightmount_collect_dependency_query_result(
    dependency: &Dependency,
    result: &mut LightmountDependencyQueryResult,
) {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::FullSelector => {
            result.add_fallback_reason(LightmountDependencyFallbackReason::FullSelector);
        },
        DependencyInvalidationKind::Normal(kind) => {
            result.add_kind(match kind {
                NormalDependencyInvalidationKind::Element => LightmountDependencyKind::Element,
                NormalDependencyInvalidationKind::ElementAndDescendants => {
                    LightmountDependencyKind::ElementAndDescendants
                },
                NormalDependencyInvalidationKind::Descendants => {
                    LightmountDependencyKind::Descendants
                },
                NormalDependencyInvalidationKind::Siblings => LightmountDependencyKind::Siblings,
                NormalDependencyInvalidationKind::SlottedElements => {
                    LightmountDependencyKind::SlottedElements
                },
                NormalDependencyInvalidationKind::Parts => LightmountDependencyKind::Parts,
            });
        },
        DependencyInvalidationKind::Relative(kind) => {
            result.add_kind(match kind {
                RelativeDependencyInvalidationKind::Ancestors => {
                    LightmountDependencyKind::RelativeAncestors
                },
                RelativeDependencyInvalidationKind::Parent => {
                    LightmountDependencyKind::RelativeParent
                },
                RelativeDependencyInvalidationKind::PrevSibling => {
                    LightmountDependencyKind::RelativePrevSibling
                },
                RelativeDependencyInvalidationKind::AncestorPrevSibling => {
                    LightmountDependencyKind::RelativeAncestorPrevSibling
                },
                RelativeDependencyInvalidationKind::EarlierSibling => {
                    LightmountDependencyKind::RelativeEarlierSibling
                },
                RelativeDependencyInvalidationKind::AncestorEarlierSibling => {
                    LightmountDependencyKind::RelativeAncestorEarlierSibling
                },
            });
        },
        DependencyInvalidationKind::Scope(_) => {
            result.add_kind(LightmountDependencyKind::Scope);
        },
    }
    if dependency.right_combinator_is_next_sibling()
        || dependency.dependency_is_relative_with_single_next_sibling()
    {
        result.add_kind(LightmountDependencyKind::Siblings);
    }
    if lightmount_dependency_has_nested_relative_dependency(dependency) {
        result.add_fallback_reason(
            LightmountDependencyFallbackReason::NestedRelativeSelectorDependency,
        );
    }
    if let Some(next) = dependency.next.as_ref() {
        for dependency in next.slice() {
            lightmount_collect_dependency_query_result(dependency, result);
        }
    }
}

fn lightmount_dependency_has_nested_relative_dependency(dependency: &Dependency) -> bool {
    if matches!(
        dependency.invalidation_kind(),
        DependencyInvalidationKind::Relative(_)
    ) {
        return false;
    }
    dependency
        .next
        .as_ref()
        .is_some_and(|next| lightmount_dependency_chain_contains_relative_dependency(next.slice()))
}

fn lightmount_dependency_chain_contains_relative_dependency(dependencies: &[Dependency]) -> bool {
    dependencies.iter().any(|dependency| {
        matches!(
            dependency.invalidation_kind(),
            DependencyInvalidationKind::Relative(_)
        ) || dependency.next.as_ref().is_some_and(|next| {
            lightmount_dependency_chain_contains_relative_dependency(next.slice())
        })
    })
}

fn lightmount_dependencies_have_sibling_sensitive(dependencies: &[Dependency]) -> bool {
    dependencies
        .iter()
        .any(lightmount_dependency_is_sibling_sensitive)
}

fn lightmount_collect_state_dependencies_from_selector_map(
    map: &SelectorMap<StateDependency>,
    summary: &mut LightmountDependencyInvalidationSummary,
) {
    for dependency in &map.root {
        lightmount_collect_state_dependency(dependency, summary);
    }
    for (_, dependencies) in map.id_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.class_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.attribute_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.local_name_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.namespace_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for dependency in &map.rare_pseudo_classes {
        lightmount_collect_state_dependency(dependency, summary);
    }
    for dependency in &map.other {
        lightmount_collect_state_dependency(dependency, summary);
    }
}

fn lightmount_collect_state_dependency(
    dependency: &StateDependency,
    summary: &mut LightmountDependencyInvalidationSummary,
) {
    summary.note_state_dependency(
        dependency.state,
        lightmount_dependency_query_result_for_dependencies(std::slice::from_ref(&dependency.dep)),
    );
}

fn lightmount_collect_state_sibling_dependencies_from_selector_map(
    map: &SelectorMap<StateDependency>,
    summary: &mut LightmountSiblingInvalidationSummary,
) {
    for dependency in &map.root {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
    for (_, dependencies) in map.id_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.class_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.attribute_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.local_name_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.namespace_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for dependency in &map.rare_pseudo_classes {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
    for dependency in &map.other {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
}

fn lightmount_collect_state_sibling_dependency(
    dependency: &StateDependency,
    summary: &mut LightmountSiblingInvalidationSummary,
) {
    if lightmount_dependency_is_sibling_sensitive(&dependency.dep) {
        summary.note_state_dependency(dependency.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::QuirksMode;
    use crate::invalidation::element::invalidation_map::note_selector_for_invalidation;
    use crate::selector_parser::SelectorParser;
    use crate::stylesheets::UrlExtraData;
    use servo_arc::ThinArc;

    fn lightmount_dependency_summary_for_selector(
        selector: &str,
    ) -> LightmountDependencyInvalidationSummary {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selectors = SelectorParser::parse_author_origin_no_namespace(selector, &url_data)
            .expect("selector should parse");
        let mut map = InvalidationMap::new();
        let mut relative_map = InvalidationMap::new();
        let mut additional_relative_map = AdditionalRelativeSelectorInvalidationMap::new();

        for selector in selectors.slice() {
            note_selector_for_invalidation(
                selector,
                QuirksMode::NoQuirks,
                &mut map,
                &mut relative_map,
                &mut additional_relative_map,
                None,
                None,
            )
            .expect("selector invalidation dependencies should collect");
        }

        let mut summary = lightmount_dependency_summary_for_invalidation_map(&map);
        summary.extend(lightmount_dependency_summary_for_invalidation_map(
            &relative_map,
        ));
        summary.extend(lightmount_dependency_summary_for_relative_invalidation_map(
            &additional_relative_map,
        ));
        summary
    }

    fn lightmount_structural_boundary_summary_for_type(
        local_name: &str,
    ) -> LightmountChildListStructuralBoundaryDependencySummary {
        let mut summary = LightmountChildListStructuralBoundaryDependencySummary::default();
        summary.note_type_dependency(LocalName::from(local_name));
        summary
    }

    fn lightmount_structural_boundary_summary_for_class(
        class: &str,
    ) -> LightmountChildListStructuralBoundaryDependencySummary {
        let mut summary = LightmountChildListStructuralBoundaryDependencySummary::default();
        summary.note_class_dependency(Atom::from(class));
        summary
    }

    fn lightmount_universal_structural_boundary_summary(
    ) -> LightmountChildListStructuralBoundaryDependencySummary {
        let mut summary = LightmountChildListStructuralBoundaryDependencySummary::default();
        summary.note_universal_dependency();
        summary
    }

    fn parse_lightmount_servo_selector(selector: &str) {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        SelectorParser::parse_author_origin_no_namespace(selector, &url_data)
            .unwrap_or_else(|error| panic!("selector should parse: {selector}: {error:?}"));
    }

    #[test]
    fn lightmount_servo_parser_accepts_migrated_selector_capabilities() {
        for selector in [
            "x-host::part(label):lang(en)",
            "x-host::part(label):dir(ltr)",
            "video:playing",
            "video:paused",
            "video:seeking",
            "video:muted",
            ":heading",
            ":heading(1, 2, 6)",
            "input:in-range",
            "input:out-of-range",
            "li:nth-child(odd of :not(.current))",
            "li:nth-last-child(2n+1 of .item)",
        ] {
            parse_lightmount_servo_selector(selector);
        }
    }

    #[test]
    fn lightmount_dependency_summary_collects_migrated_state_pseudos() {
        let summary = lightmount_dependency_summary_for_selector(
            "video:playing, video:paused, video:seeking, video:muted, \
             :heading, :heading(1, 2, 6), input:in-range, input:out-of-range",
        );

        for state in [
            ElementState::PAUSED,
            ElementState::SEEKING,
            ElementState::MUTED,
            ElementState::HEADING_LEVEL_BITS,
            ElementState::INRANGE,
            ElementState::OUTOFRANGE,
        ] {
            let result = summary.query_state(state);
            assert!(
                result.has_any_dependency(),
                "missing dependency for {state:?}"
            );
            assert!(
                result.fallback_reasons().is_empty(),
                "state dependency should not require fallback for {state:?}: {:?}",
                result.fallback_reasons()
            );
        }
    }

    #[test]
    fn lightmount_dependency_summary_collects_lang_and_dir_attribute_pseudos() {
        let summary = lightmount_dependency_summary_for_selector(
            "x-host::part(label):lang(fr), x-host::part(label):dir(rtl)",
        );

        for attribute in ["lang", "dir"] {
            let result = summary.query_attribute(&LocalName::from(attribute));
            assert!(
                result.has_any_dependency(),
                "missing dependency for {attribute}"
            );
            assert!(
                result.fallback_reasons().is_empty(),
                "attribute dependency should not require fallback for {attribute}: {:?}",
                result.fallback_reasons()
            );
        }
    }

    #[test]
    fn lightmount_dependency_query_result_keeps_fallback_reasons_out_of_kinds() {
        let mut result = LightmountDependencyQueryResult::default();
        result.add_fallback_reason(LightmountDependencyFallbackReason::FullSelector);

        assert!(result.has_any_dependency());
        assert!(result.requires_fallback());
        assert_eq!(
            result.fallback_reasons(),
            &[LightmountDependencyFallbackReason::FullSelector]
        );
        assert_eq!(
            result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::SourceFallback
        );
        assert!(result.kinds().is_empty());
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_source_fallback_handling() {
        let mut nth_of = LightmountDependencyQueryResult::default();
        nth_of.add_fallback_reason(LightmountDependencyFallbackReason::NthOfDependency);
        let LightmountDependencyFallbackHandling::ContextRoots(reasons) =
            nth_of.source_dependency_fallback_handling()
        else {
            panic!("nth-of dependency should use context fallback roots");
        };
        assert!(reasons.contains(&LightmountSourceInvalidationFallbackReason::NthOfDependency));

        let mut scope = LightmountDependencyQueryResult::default();
        scope.add_kind(LightmountDependencyKind::Scope);
        let LightmountDependencyFallbackHandling::SourceFallback(reasons) =
            scope.source_dependency_fallback_handling()
        else {
            panic!("scope dependency should require source fallback roots");
        };
        assert!(reasons.contains(&LightmountSourceInvalidationFallbackReason::ScopeDependency));
    }

    #[test]
    fn lightmount_dependency_query_result_dedupes_extended_fallback_reasons() {
        let mut first = LightmountDependencyQueryResult::default();
        first.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        let mut second = LightmountDependencyQueryResult::default();
        second.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        second.add_kind(LightmountDependencyKind::Siblings);

        first.extend(second);

        assert!(first.has_any_dependency());
        assert!(first.requires_fallback());
        assert_eq!(
            first.fallback_reasons(),
            &[LightmountDependencyFallbackReason::UnknownDependency]
        );
        assert_eq!(first.kinds(), &[LightmountDependencyKind::Siblings]);
        assert!(first.has_sibling_dependency());
    }

    #[test]
    fn lightmount_dependency_query_result_derives_shape_fallback_reasons() {
        let mut scope = LightmountDependencyQueryResult::default();
        scope.add_kind(LightmountDependencyKind::Scope);
        assert_eq!(
            scope.fallback_or_shape_reasons(),
            vec![LightmountDependencyFallbackReason::ScopeDependency]
        );

        let mut sibling = LightmountDependencyQueryResult::default();
        sibling.add_kind(LightmountDependencyKind::Siblings);
        assert_eq!(
            sibling.fallback_or_shape_reasons(),
            vec![LightmountDependencyFallbackReason::UnsupportedDependency]
        );
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_relative_shape_predicates() {
        let mut direct_previous = LightmountDependencyQueryResult::default();
        direct_previous.add_kind(LightmountDependencyKind::RelativePrevSibling);
        direct_previous.add_kind(LightmountDependencyKind::Siblings);
        assert!(direct_previous.has_relative_selector_dependency());
        assert!(direct_previous.has_relative_previous_sibling_dependency());
        assert!(direct_previous.has_only_direct_relative_previous_sibling_dependency());

        let mut ancestor_previous = LightmountDependencyQueryResult::default();
        ancestor_previous.add_kind(LightmountDependencyKind::RelativeAncestorPrevSibling);
        assert!(ancestor_previous.has_relative_selector_dependency());
        assert!(ancestor_previous.has_relative_previous_sibling_dependency());
        assert!(!ancestor_previous.has_only_direct_relative_previous_sibling_dependency());

        let mut ancestor = LightmountDependencyQueryResult::default();
        ancestor.add_kind(LightmountDependencyKind::RelativeAncestors);
        assert!(ancestor.has_relative_selector_dependency());
        assert!(!ancestor.has_relative_previous_sibling_dependency());
        assert!(!ancestor.has_only_direct_relative_previous_sibling_dependency());
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_context_root_flags() {
        #[derive(Default)]
        struct Sink {
            calls: Vec<&'static str>,
        }

        impl LightmountDependencyContextRootFlagsSink for Sink {
            fn require_context_source_fallback(&mut self) {
                self.calls.push("source_fallback");
            }

            fn include_context_local_subtree(&mut self) {
                self.calls.push("local_subtree");
            }

            fn include_context_ancestor_chain(&mut self) {
                self.calls.push("ancestor_chain");
            }

            fn include_context_following_siblings(&mut self) {
                self.calls.push("following_siblings");
            }

            fn include_context_ancestor_following_siblings(&mut self) {
                self.calls.push("ancestor_following_siblings");
            }

            fn include_context_previous_sibling(&mut self) {
                self.calls.push("previous_sibling");
            }

            fn include_context_earlier_siblings(&mut self) {
                self.calls.push("earlier_siblings");
            }

            fn include_context_ancestor_previous_siblings(&mut self) {
                self.calls.push("ancestor_previous_siblings");
            }

            fn include_context_slotted_elements(&mut self) {
                self.calls.push("slotted_elements");
            }

            fn include_context_parts(&mut self) {
                self.calls.push("parts");
            }
        }

        let mut query = LightmountDependencyQueryResult::default();
        query.add_kind(LightmountDependencyKind::ElementAndDescendants);
        query.add_kind(LightmountDependencyKind::Siblings);
        query.add_kind(LightmountDependencyKind::SlottedElements);
        query.add_kind(LightmountDependencyKind::Parts);
        query.add_kind(LightmountDependencyKind::RelativePrevSibling);
        query.add_kind(LightmountDependencyKind::RelativeAncestorEarlierSibling);
        let flags = query.context_root_flags();
        let mut sink = Sink::default();
        flags.drain_into(false, &mut sink);

        assert_eq!(
            sink.calls,
            vec![
                "local_subtree",
                "ancestor_chain",
                "ancestor_following_siblings",
                "previous_sibling",
                "ancestor_previous_siblings",
                "slotted_elements",
                "parts"
            ]
        );

        query.add_kind(LightmountDependencyKind::Scope);
        let mut sink = Sink::default();
        query.context_root_flags().drain_into(false, &mut sink);
        assert!(sink.calls.contains(&"source_fallback"));
    }

    #[test]
    fn lightmount_dependency_invalidation_context_roots_drains_query_into_typed_roots() {
        #[derive(Default)]
        struct Sink {
            requires_source_fallback: bool,
            roots: Vec<u32>,
        }

        impl LightmountDependencyContextRootFlagsSink for Sink {
            fn require_context_source_fallback(&mut self) {
                self.requires_source_fallback = true;
            }

            fn include_context_local_subtree(&mut self) {
                self.roots.push(1);
            }

            fn include_context_ancestor_chain(&mut self) {
                self.roots.push(2);
            }

            fn include_context_following_siblings(&mut self) {
                self.roots.push(3);
            }

            fn include_context_ancestor_following_siblings(&mut self) {
                self.roots.push(4);
            }

            fn include_context_previous_sibling(&mut self) {
                self.roots.push(5);
            }

            fn include_context_earlier_siblings(&mut self) {
                self.roots.push(6);
            }

            fn include_context_ancestor_previous_siblings(&mut self) {
                self.roots.push(7);
            }

            fn include_context_slotted_elements(&mut self) {
                self.roots.push(8);
            }

            fn include_context_parts(&mut self) {
                self.roots.push(9);
            }
        }

        impl LightmountDependencyInvalidationContextRootsSink<u32> for Sink {
            fn drain_collected_context_roots_into(
                self,
                target: &mut impl LightmountDependencyInvalidationContextRootsPartsSink<u32>,
            ) {
                if self.requires_source_fallback {
                    target.record_context_source_fallback();
                }
                target.extend_context_roots(self.roots);
            }
        }

        let mut query = LightmountDependencyQueryResult::default();
        query.add_kind(LightmountDependencyKind::Element);
        query.add_kind(LightmountDependencyKind::Siblings);
        query.add_kind(LightmountDependencyKind::Scope);

        let roots = lightmount_dependency_invalidation_context_roots(&query, true, Sink::default());

        assert!(roots.requires_source_fallback());
        assert_eq!(roots.roots(), &[1, 3]);
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_structural_context_cleanup_policy() {
        let mut query = LightmountDependencyQueryResult::default();
        query.add_kind(LightmountDependencyKind::RelativePrevSibling);
        assert!(query.requires_structural_context_fallback_cleanup(true, true));
        assert!(!query.requires_structural_context_fallback_cleanup(false, true));
        assert!(!query.requires_structural_context_fallback_cleanup(true, false));

        let mut non_relative = LightmountDependencyQueryResult::default();
        non_relative.add_kind(LightmountDependencyKind::Element);
        assert!(!non_relative.requires_structural_context_fallback_cleanup(true, true));
    }

    #[test]
    fn lightmount_dependency_fallback_reason_maps_raw_dependency_kind() {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selector = SelectorParser::parse_author_origin_no_namespace(".subject", &url_data)
            .expect("selector should parse")
            .slice()[0]
            .clone();
        let dependency_for_kind = |kind| Dependency::new(selector.clone(), 0, None, kind);

        assert_eq!(
            lightmount_dependency_fallback_reason_for_dependency(&dependency_for_kind(
                DependencyInvalidationKind::FullSelector
            )),
            LightmountDependencyFallbackReason::FullSelector
        );
        assert_eq!(
            lightmount_dependency_fallback_reason_for_dependency(&dependency_for_kind(
                DependencyInvalidationKind::Relative(RelativeDependencyInvalidationKind::Ancestors)
            )),
            LightmountDependencyFallbackReason::RelativeAnySelector
        );
        assert_eq!(
            lightmount_relative_selector_invalidation_fallback_reason(
                RelativeDependencyInvalidationKind::Ancestors,
                &dependency_for_kind(DependencyInvalidationKind::Relative(
                    RelativeDependencyInvalidationKind::Ancestors
                ))
            ),
            LightmountSourceInvalidationFallbackReason::RelativeAnySelector
        );
        assert_eq!(
            lightmount_dependency_fallback_reason_for_dependency(&dependency_for_kind(
                DependencyInvalidationKind::Scope(ScopeDependencyInvalidationKind::ScopeEnd)
            )),
            LightmountDependencyFallbackReason::ScopeDependency
        );
        assert_eq!(
            lightmount_dependency_fallback_reason_for_dependency(&dependency_for_kind(
                DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Element)
            )),
            LightmountDependencyFallbackReason::UnsupportedDependency
        );
    }

    #[test]
    fn lightmount_dependency_invalidation_action_maps_raw_dependency_kind() {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selector = SelectorParser::parse_author_origin_no_namespace(".subject", &url_data)
            .expect("selector should parse")
            .slice()[0]
            .clone();
        let dependency_for_kind = |kind| Dependency::new(selector.clone(), 0, None, kind);

        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Element)
            )),
            LightmountDependencyInvalidationAction::Element
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Siblings)
            )),
            LightmountDependencyInvalidationAction::Siblings
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::FullSelector
            )),
            LightmountDependencyInvalidationAction::Fallback(
                LightmountSourceInvalidationFallbackReason::FullSelector
            )
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Relative(RelativeDependencyInvalidationKind::Ancestors)
            )),
            LightmountDependencyInvalidationAction::Fallback(
                LightmountSourceInvalidationFallbackReason::RelativeAnySelector
            )
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Scope(ScopeDependencyInvalidationKind::ImplicitScope)
            )),
            LightmountDependencyInvalidationAction::Scope(
                LightmountScopeDependencyInvalidationAction::ImplicitScope
            )
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Scope(ScopeDependencyInvalidationKind::ScopeEnd)
            )),
            LightmountDependencyInvalidationAction::Scope(
                LightmountScopeDependencyInvalidationAction::ForceAtSubject { force_add: false }
            )
        );
        assert_eq!(
            lightmount_dependency_invalidation_action(&dependency_for_kind(
                DependencyInvalidationKind::Scope(ScopeDependencyInvalidationKind::ExplicitScope)
            )),
            LightmountDependencyInvalidationAction::Scope(
                LightmountScopeDependencyInvalidationAction::CheckNextInScope
            )
        );
    }

    #[test]
    fn lightmount_dependency_invalidation_action_drains_into_sink() {
        #[derive(Default)]
        struct Sink {
            calls: Vec<&'static str>,
            fallback_reason: Option<LightmountSourceInvalidationFallbackReason>,
            scope_action: Option<LightmountScopeDependencyInvalidationAction>,
        }

        impl LightmountDependencyInvalidationActionSink for Sink {
            fn invalidate_element(&mut self) {
                self.calls.push("element");
            }

            fn invalidate_element_and_descendants(&mut self) {
                self.calls.push("element_and_descendants");
            }

            fn invalidate_descendants(&mut self) {
                self.calls.push("descendants");
            }

            fn invalidate_siblings(&mut self) {
                self.calls.push("siblings");
            }

            fn invalidate_slotted_elements(&mut self) {
                self.calls.push("slotted");
            }

            fn invalidate_parts(&mut self) {
                self.calls.push("parts");
            }

            fn invalidate_fallback(&mut self, reason: LightmountSourceInvalidationFallbackReason) {
                self.fallback_reason = Some(reason);
            }

            fn invalidate_scope(&mut self, action: LightmountScopeDependencyInvalidationAction) {
                self.scope_action = Some(action);
            }
        }

        let mut sink = Sink::default();
        LightmountDependencyInvalidationAction::ElementAndDescendants.drain_into(&mut sink);
        LightmountDependencyInvalidationAction::Fallback(
            LightmountSourceInvalidationFallbackReason::FullSelector,
        )
        .drain_into(&mut sink);
        LightmountDependencyInvalidationAction::Scope(
            LightmountScopeDependencyInvalidationAction::CheckNextInScope,
        )
        .drain_into(&mut sink);

        assert_eq!(sink.calls, vec!["element_and_descendants"]);
        assert_eq!(
            sink.fallback_reason,
            Some(LightmountSourceInvalidationFallbackReason::FullSelector)
        );
        assert_eq!(
            sink.scope_action,
            Some(LightmountScopeDependencyInvalidationAction::CheckNextInScope)
        );
    }

    #[test]
    fn lightmount_scope_dependency_invalidation_action_drains_into_sink() {
        #[derive(Default)]
        struct Sink {
            calls: Vec<&'static str>,
            force_add: Option<bool>,
        }

        impl LightmountScopeDependencyInvalidationActionSink for Sink {
            fn invalidate_implicit_scope(&mut self) {
                self.calls.push("implicit_scope");
            }

            fn invalidate_scope_force_at_subject(&mut self, force_add: bool) {
                self.calls.push("force_at_subject");
                self.force_add = Some(force_add);
            }

            fn invalidate_scope_check_next(&mut self) {
                self.calls.push("check_next");
            }

            fn invalidate_scope_by_combinator(&mut self) {
                self.calls.push("by_combinator");
            }
        }

        let mut sink = Sink::default();
        LightmountScopeDependencyInvalidationAction::ImplicitScope.drain_into(&mut sink);
        LightmountScopeDependencyInvalidationAction::ForceAtSubject { force_add: true }
            .drain_into(&mut sink);
        LightmountScopeDependencyInvalidationAction::CheckNextInScope.drain_into(&mut sink);
        LightmountScopeDependencyInvalidationAction::PushByCombinator.drain_into(&mut sink);

        assert_eq!(
            sink.calls,
            vec![
                "implicit_scope",
                "force_at_subject",
                "check_next",
                "by_combinator"
            ]
        );
        assert_eq!(sink.force_add, Some(true));
    }

    #[test]
    fn lightmount_relative_dependency_invalidation_action_drains_into_sink() {
        #[derive(Default)]
        struct Sink {
            calls: Vec<&'static str>,
        }

        impl LightmountRelativeDependencyInvalidationActionSink for Sink {
            fn visit_relative_ancestor_candidates(&mut self) {
                self.calls.push("ancestors");
            }

            fn visit_relative_parent_candidate(&mut self) {
                self.calls.push("parent");
            }

            fn visit_relative_previous_sibling_candidate(&mut self) {
                self.calls.push("prev_sibling");
            }

            fn visit_relative_earlier_sibling_candidates(&mut self) {
                self.calls.push("earlier_sibling");
            }

            fn visit_relative_ancestor_previous_sibling_candidates(&mut self) {
                self.calls.push("ancestor_prev_sibling");
            }

            fn visit_relative_ancestor_earlier_sibling_candidates(&mut self) {
                self.calls.push("ancestor_earlier_sibling");
            }
        }

        let mut sink = Sink::default();
        LightmountRelativeDependencyInvalidationAction::Ancestors.drain_into(&mut sink);
        LightmountRelativeDependencyInvalidationAction::Parent.drain_into(&mut sink);
        LightmountRelativeDependencyInvalidationAction::PrevSibling.drain_into(&mut sink);
        LightmountRelativeDependencyInvalidationAction::EarlierSibling.drain_into(&mut sink);
        LightmountRelativeDependencyInvalidationAction::AncestorPrevSibling.drain_into(&mut sink);
        LightmountRelativeDependencyInvalidationAction::AncestorEarlierSibling
            .drain_into(&mut sink);

        assert_eq!(
            sink.calls,
            vec![
                "ancestors",
                "parent",
                "prev_sibling",
                "earlier_sibling",
                "ancestor_prev_sibling",
                "ancestor_earlier_sibling"
            ]
        );
    }

    #[test]
    fn lightmount_relative_dependency_helpers_map_raw_dependency_kind() {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selector = SelectorParser::parse_author_origin_no_namespace(".subject", &url_data)
            .expect("selector should parse")
            .slice()[0]
            .clone();
        let dependency_for_kind = |kind| Dependency::new(selector.clone(), 0, None, kind);

        let relative = dependency_for_kind(DependencyInvalidationKind::Relative(
            RelativeDependencyInvalidationKind::AncestorEarlierSibling,
        ));
        let normal = dependency_for_kind(DependencyInvalidationKind::Normal(
            NormalDependencyInvalidationKind::Descendants,
        ));
        let full = dependency_for_kind(DependencyInvalidationKind::FullSelector);

        assert_eq!(
            lightmount_relative_dependency_invalidation_action(&relative),
            Some(LightmountRelativeDependencyInvalidationAction::AncestorEarlierSibling)
        );
        assert!(lightmount_dependency_is_relative_selector(&relative));
        assert!(!lightmount_dependency_is_relative_selector(&normal));
        assert!(lightmount_snapshot_relative_outer_dependency_supported(
            &normal
        ));
        assert!(!lightmount_snapshot_relative_outer_dependency_supported(
            &relative
        ));
        assert!(!lightmount_snapshot_relative_outer_dependency_supported(
            &full
        ));
    }

    #[test]
    fn lightmount_source_fallback_reason_preserves_dependency_detail() {
        let cases = [
            (
                LightmountDependencyFallbackReason::UnknownDependency,
                LightmountSourceInvalidationFallbackReason::UnknownDependency,
            ),
            (
                LightmountDependencyFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::FullSelector,
            ),
            (
                LightmountDependencyFallbackReason::RelativeAnySelector,
                LightmountSourceInvalidationFallbackReason::RelativeAnySelector,
            ),
            (
                LightmountDependencyFallbackReason::ScopeDependency,
                LightmountSourceInvalidationFallbackReason::ScopeDependency,
            ),
            (
                LightmountDependencyFallbackReason::UnsupportedStateDependency,
                LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency,
            ),
            (
                LightmountDependencyFallbackReason::UnsupportedDependency,
                LightmountSourceInvalidationFallbackReason::UnsupportedDependency,
            ),
            (
                LightmountDependencyFallbackReason::NthOfDependency,
                LightmountSourceInvalidationFallbackReason::NthOfDependency,
            ),
            (
                LightmountDependencyFallbackReason::NestedRelativeSelectorDependency,
                LightmountSourceInvalidationFallbackReason::NestedRelativeSelectorDependency,
            ),
        ];

        for (dependency_reason, source_reason) in cases {
            assert_eq!(
                LightmountSourceInvalidationFallbackReason::from(dependency_reason),
                source_reason
            );
        }
    }

    #[test]
    fn lightmount_attribute_and_state_runtime_policy_is_fork_owned() {
        assert!(lightmount_attribute_change_can_use_retained_invalidator(
            "class", false
        ));
        assert!(lightmount_attribute_change_can_use_retained_invalidator(
            "style", false
        ));
        assert!(!lightmount_attribute_change_can_use_retained_invalidator(
            "width", true
        ));

        assert!(lightmount_attribute_change_can_skip_fallback_without_dependency("class"));
        assert!(lightmount_attribute_change_can_skip_fallback_without_dependency("data-state"));
        assert!(lightmount_attribute_change_can_skip_fallback_without_dependency("aria-expanded"));
        assert!(lightmount_attribute_change_can_skip_fallback_without_dependency("lang"));
        assert!(lightmount_attribute_change_can_skip_fallback_without_dependency("dir"));
        assert!(!lightmount_attribute_change_can_skip_fallback_without_dependency("DATA-State"));
        assert!(!lightmount_attribute_change_can_skip_fallback_without_dependency("href"));

        for state in [
            ElementState::CHECKED,
            ElementState::INDETERMINATE,
            ElementState::PLACEHOLDER_SHOWN,
            ElementState::DEFINED,
            ElementState::PAUSED,
            ElementState::MUTED,
            ElementState::SEEKING,
        ] {
            assert!(lightmount_state_change_can_use_retained_invalidator(
                state, None
            ));
            assert_eq!(
                lightmount_source_fallback_reason_for_unretained_state_change(state, None),
                None
            );
        }

        assert!(!lightmount_state_change_can_use_retained_invalidator(
            ElementState::HOVER,
            None
        ));
        assert_eq!(
            lightmount_source_fallback_reason_for_unretained_state_change(
                ElementState::HOVER,
                None
            ),
            Some(LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency)
        );
        assert!(lightmount_state_change_can_use_retained_invalidator(
            ElementState::HOVER,
            Some(ElementState::empty())
        ));
    }

    #[test]
    fn lightmount_runtime_fallback_roots_for_mutation_inputs_are_fork_planned() {
        struct Resolver;

        impl LightmountRuntimeFallbackRootResolver<u32> for Resolver {
            fn unknown_slot_assignment_fallback_root(&self, slot: u32) -> u32 {
                slot + 100
            }
        }

        let added_nodes = [3, 5];
        let roots = lightmount_runtime_fallback_roots_for_mutation_inputs(
            [
                LightmountRuntimeFallbackRootInput::Attribute {
                    element: 1,
                    attribute_name: "class",
                    has_dependency_change: true,
                    has_non_css_runtime_side_effect: false,
                },
                LightmountRuntimeFallbackRootInput::Attribute {
                    element: 2,
                    attribute_name: "width",
                    has_dependency_change: true,
                    has_non_css_runtime_side_effect: true,
                },
                LightmountRuntimeFallbackRootInput::ChildList {
                    added_nodes: &added_nodes,
                },
                LightmountRuntimeFallbackRootInput::SlotAssignment {
                    slot: 4,
                    has_assignment_snapshot: false,
                },
                LightmountRuntimeFallbackRootInput::ConnectedSubtree { root: 2 },
                LightmountRuntimeFallbackRootInput::OtherMutation,
            ],
            &Resolver,
        );

        assert_eq!(roots, vec![2, 3, 5, 104]);

        let child_list_only = lightmount_runtime_fallback_roots_for_mutation_inputs(
            [LightmountRuntimeFallbackRootInput::ChildList {
                added_nodes: &added_nodes,
            }],
            &Resolver,
        );
        assert!(child_list_only.is_empty());

        let known_slot = lightmount_runtime_fallback_roots_for_mutation_inputs(
            [LightmountRuntimeFallbackRootInput::SlotAssignment {
                slot: 4,
                has_assignment_snapshot: true,
            }],
            &Resolver,
        );
        assert!(known_slot.is_empty());
    }

    #[test]
    fn lightmount_retained_source_invalidation_kind_exposes_result_policy() {
        use LightmountRetainedSourceStyleInvalidationKind::{
            ContextFallback, FallbackOnly, MissingFallbackRoots, RetainedQueries,
            SourceScopeFallback,
        };

        assert_eq!(
            ContextFallback.merged_with(ContextFallback),
            ContextFallback
        );
        assert_eq!(
            lightmount_merge_retained_source_invalidation_kind(ContextFallback, ContextFallback),
            ContextFallback
        );
        assert_eq!(ContextFallback.merged_with(FallbackOnly), FallbackOnly);
        assert_eq!(
            lightmount_merge_retained_source_invalidation_fallback_kind(
                Some(ContextFallback),
                Some(FallbackOnly),
            ),
            Some(FallbackOnly)
        );
        assert_eq!(
            lightmount_merge_retained_source_invalidation_fallback_kind(None, Some(FallbackOnly)),
            Some(FallbackOnly)
        );
        assert_eq!(
            lightmount_merge_retained_source_invalidation_fallback_kind(
                Some(ContextFallback),
                None,
            ),
            Some(ContextFallback)
        );
        assert_eq!(
            FallbackOnly.merged_with(SourceScopeFallback),
            SourceScopeFallback
        );
        assert_eq!(
            SourceScopeFallback.merged_with(MissingFallbackRoots),
            MissingFallbackRoots
        );
        assert_eq!(
            MissingFallbackRoots.merged_with(RetainedQueries),
            RetainedQueries
        );
        assert!(RetainedQueries.carries_retained_queries());
        assert!(!FallbackOnly.carries_retained_queries());
        assert!(FallbackOnly.can_target_fallback_root());
        assert!(SourceScopeFallback.can_target_fallback_root());
        assert!(!ContextFallback.can_target_fallback_root());

        assert_eq!(
            ContextFallback.fallback_source_result_kind(true),
            LightmountSourceStyleInvalidationSourceResultKind::ContextFallback
        );
        assert_eq!(
            FallbackOnly.fallback_source_result_kind(false),
            LightmountSourceStyleInvalidationSourceResultKind::FallbackOnly
        );
        assert_eq!(
            FallbackOnly.fallback_source_result_kind(true),
            LightmountSourceStyleInvalidationSourceResultKind::Fallback
        );
        assert_eq!(
            LightmountSourceFallbackRootAvailability::for_root_count(0),
            None
        );
        assert_eq!(
            LightmountSourceFallbackRootAvailability::for_root_count(2),
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 2 })
        );
        assert_eq!(FallbackOnly.fallback_root_availability(0), None);
        assert_eq!(
            FallbackOnly.fallback_root_availability(2),
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 2 })
        );
        assert_eq!(
            MissingFallbackRoots.fallback_root_availability(0),
            Some(LightmountSourceFallbackRootAvailability::Missing)
        );
        assert_eq!(
            MissingFallbackRoots.fallback_root_availability(2),
            Some(LightmountSourceFallbackRootAvailability::Missing)
        );
        assert_eq!(
            SourceScopeFallback.fallback_reason(),
            Some(LightmountSourceInvalidationFallbackReason::SourceScopeFallback)
        );
        assert_eq!(
            MissingFallbackRoots.fallback_reason(),
            Some(LightmountSourceInvalidationFallbackReason::MissingFallbackRoots)
        );
        assert_eq!(FallbackOnly.fallback_reason(), None);
    }

    #[test]
    fn lightmount_retained_style_query_maps_to_stylo_query_shape() {
        let traversal = LightmountRetainedStyleSiblingTraversal::new(Some(1_u32), Some(3_u32));
        let class_query = LightmountRetainedStyleInvalidationQuery::class(2_u32, "active".into())
            .with_sibling_traversal(Some(traversal));

        assert_eq!(class_query.root(), 2);
        assert_eq!(class_query.sibling_traversal(), Some(traversal));
        assert!(!class_query.is_universal());
        assert!(!class_query.allows_direct_previous_following_sibling_fallback());
        assert_eq!(
            class_query.as_stylo_query(),
            LightmountStyleInvalidationQuery::Class("active")
        );
        let source_query = class_query.as_source_query();
        assert_eq!(source_query.root(), 2);
        assert_eq!(
            source_query.query(),
            LightmountStyleInvalidationQuery::Class("active")
        );
        assert_eq!(source_query.previous_sibling(), Some(1));
        assert_eq!(source_query.next_sibling(), Some(3));
        assert_eq!(traversal.previous_sibling(), Some(1));
        assert_eq!(traversal.next_sibling(), Some(3));

        let universal_query = LightmountRetainedStyleInvalidationQuery::universal(7_u32);
        assert!(universal_query.is_universal());
        assert_eq!(
            universal_query.as_stylo_query(),
            LightmountStyleInvalidationQuery::Universal
        );
        assert_eq!(universal_query.as_source_query().previous_sibling(), None);
        assert_eq!(universal_query.as_source_query().next_sibling(), None);

        let heading_query = LightmountRetainedStyleInvalidationQuery::state(
            9_u32,
            ElementState::HEADING_LEVEL_BITS,
        );
        assert!(heading_query.allows_direct_previous_following_sibling_fallback());
        assert_eq!(
            heading_query.as_stylo_query(),
            LightmountStyleInvalidationQuery::State(ElementState::HEADING_LEVEL_BITS)
        );
    }

    #[test]
    fn lightmount_element_dependency_snapshot_builds_retained_queries() {
        let traversal = LightmountRetainedStyleSiblingTraversal::new(Some(1_u32), Some(3_u32));
        let snapshot = LightmountElementDependencySnapshot::new(
            2_u32,
            "article".into(),
            ElementState::CHECKED,
            vec!["class".into(), "data-state".into()],
            vec!["active".into()],
            vec!["expanded".into()],
            Some("main".into()),
        );

        let queries =
            lightmount_retained_queries_for_element_dependency_snapshot(&snapshot, Some(traversal));
        let query_shapes = queries
            .iter()
            .map(|query| query.as_stylo_query())
            .collect::<Vec<_>>();
        assert_eq!(
            query_shapes,
            vec![
                LightmountStyleInvalidationQuery::Universal,
                LightmountStyleInvalidationQuery::Type("article"),
                LightmountStyleInvalidationQuery::State(ElementState::CHECKED),
                LightmountStyleInvalidationQuery::Attribute("class"),
                LightmountStyleInvalidationQuery::Attribute("data-state"),
                LightmountStyleInvalidationQuery::Class("active"),
                LightmountStyleInvalidationQuery::Id("main"),
                LightmountStyleInvalidationQuery::CustomState("expanded"),
            ]
        );
        assert!(queries
            .iter()
            .all(|query| query.sibling_traversal() == Some(traversal)));
        assert_eq!(snapshot.handle(), 2);
        assert_eq!(snapshot.class_tokens(), &["active".to_string()]);

        let non_universal =
            lightmount_retained_non_universal_queries_for_element_dependency_snapshot(
                &snapshot, None,
            );
        assert!(non_universal.iter().all(|query| !query.is_universal()));
        assert_eq!(
            non_universal[0].as_stylo_query(),
            LightmountStyleInvalidationQuery::Type("article")
        );
        assert!(non_universal
            .iter()
            .all(|query| query.sibling_traversal().is_none()));
    }

    #[test]
    fn lightmount_retained_source_invalidation_input_selects_typed_variant() {
        #[derive(Default)]
        struct Sink {
            retained_fallback_kind: Option<Option<LightmountRetainedSourceStyleInvalidationKind>>,
            retained_shadow_root: Option<u32>,
            retained_query_count: usize,
            retained_reasoned_roots: Vec<u32>,
            retained_exact_safety_roots: Vec<u32>,
            retained_fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
            retained_snapshot: Option<u8>,
            fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
            fallback_roots: Vec<u32>,
            fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        }

        impl<'a> LightmountRetainedSourceStyleInvalidationSink<'a, u32, u8> for Sink {
            fn run_retained_source_style_invalidation_queries(
                &mut self,
                fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
                cascade_data: Option<&'a ServoArc<CascadeData>>,
                shadow_root: Option<u32>,
                queries: &'a IndexSet<LightmountRetainedStyleInvalidationQuery<u32>>,
                reasoned_fallback_roots: &'a IndexSet<u32>,
                exact_safety_fallback_roots: &'a IndexSet<u32>,
                fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
                mutation_snapshot: &'a u8,
            ) {
                assert!(cascade_data.is_none());
                self.retained_fallback_kind = Some(fallback_kind);
                self.retained_shadow_root = shadow_root;
                self.retained_query_count = queries.len();
                self.retained_reasoned_roots
                    .extend(reasoned_fallback_roots.iter().copied());
                self.retained_exact_safety_roots
                    .extend(exact_safety_fallback_roots.iter().copied());
                self.retained_fallback_reasons
                    .extend(fallback_reasons.iter().copied());
                self.retained_snapshot = Some(*mutation_snapshot);
            }

            fn run_fallback_source_style_invalidation(
                &mut self,
                kind: LightmountRetainedSourceStyleInvalidationKind,
                fallback_roots: &'a IndexSet<u32>,
                fallback_reasons: &'a IndexSet<LightmountSourceInvalidationFallbackReason>,
            ) {
                self.fallback_kind = Some(kind);
                self.fallback_roots.extend(fallback_roots.iter().copied());
                self.fallback_reasons
                    .extend(fallback_reasons.iter().copied());
            }
        }

        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let queries = IndexSet::from([query]);
        let reasoned_roots = IndexSet::from([2_u32]);
        let exact_safety_roots = IndexSet::from([3_u32]);
        let fallback_reasons =
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector]);
        let snapshot = 7_u8;

        let retained = lightmount_retained_source_style_invalidation_from_parts(
            LightmountRetainedSourceStyleInvalidationKind::RetainedQueries,
            Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
            None,
            Some(4_u32),
            Some(&queries),
            &reasoned_roots,
            &exact_safety_roots,
            &fallback_reasons,
            &snapshot,
        );

        let mut sink = Sink::default();
        retained.drain_into(&mut sink);
        assert_eq!(
            sink.retained_fallback_kind,
            Some(Some(
                LightmountRetainedSourceStyleInvalidationKind::ContextFallback
            ))
        );
        assert_eq!(sink.retained_shadow_root, Some(4));
        assert_eq!(sink.retained_query_count, 1);
        assert_eq!(sink.retained_reasoned_roots, vec![2]);
        assert_eq!(sink.retained_exact_safety_roots, vec![3]);
        assert_eq!(
            sink.retained_fallback_reasons,
            vec![LightmountSourceInvalidationFallbackReason::FullSelector]
        );
        assert_eq!(sink.retained_snapshot, Some(snapshot));

        let fallback = lightmount_retained_source_style_invalidation_from_parts(
            LightmountRetainedSourceStyleInvalidationKind::FallbackOnly,
            None,
            None,
            None,
            None,
            &reasoned_roots,
            &exact_safety_roots,
            &fallback_reasons,
            &snapshot,
        );
        let mut sink = Sink::default();
        fallback.drain_into(&mut sink);
        assert_eq!(
            sink.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly)
        );
        assert_eq!(sink.fallback_roots, vec![2]);
        assert_eq!(
            sink.fallback_reasons,
            vec![LightmountSourceInvalidationFallbackReason::FullSelector]
        );
    }

    #[test]
    fn lightmount_source_dependency_request_requirement_merges_gates() {
        let exact = LightmountSourceDependencyRequestRequirement::exact();
        let structural = LightmountSourceDependencyRequestRequirement::child_list_structural();
        let relative = LightmountSourceDependencyRequestRequirement::relative_previous_sibling();
        let both =
            LightmountSourceDependencyRequestRequirement::child_list_structural_relative_previous_sibling();

        assert!(!exact.requires_child_list_structural_dependency());
        assert!(!exact.requires_relative_previous_sibling_dependency());
        assert!(structural.requires_child_list_structural_dependency());
        assert!(!structural.requires_relative_previous_sibling_dependency());
        assert!(!relative.requires_child_list_structural_dependency());
        assert!(relative.requires_relative_previous_sibling_dependency());
        assert!(both.requires_child_list_structural_dependency());
        assert!(both.requires_relative_previous_sibling_dependency());

        let merged = structural.merged_with(relative);
        assert!(!merged.requires_child_list_structural_dependency());
        assert!(merged.requires_relative_previous_sibling_dependency());

        let merged = both.merged_with(structural);
        assert!(merged.requires_child_list_structural_dependency());
        assert!(merged.requires_relative_previous_sibling_dependency());
    }

    #[test]
    fn lightmount_source_dependency_request_exposes_typed_context_and_gates() {
        let query = LightmountRetainedStyleInvalidationQuery::id(1_u32, "target".into());
        let context = LightmountDependencyInvalidationFallbackContext::from_mutation_relation(
            Some(2),
            Some(3),
            Some(4),
        );
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            Some(context),
            LightmountSourceDependencyRequestRequirement::child_list_structural_relative_previous_sibling(),
        );

        assert_eq!(request.query().root(), 1);
        assert!(request.requires_child_list_structural_dependency());
        assert!(request.requires_relative_previous_sibling_dependency());
        let context = request.context().expect("request should expose context");
        assert_eq!(context.parent(), Some(2));
        assert_eq!(context.previous_sibling(), Some(3));
        assert_eq!(context.next_sibling(), Some(4));

        let empty = LightmountDependencyInvalidationFallbackContext::<u32>::default();
        assert_eq!(empty.parent(), None);
        assert_eq!(empty.previous_sibling(), None);
        assert_eq!(empty.next_sibling(), None);

        let exact_safety_roots = LightmountDependencyInvalidationContextRoots::new(false, vec![5]);
        assert!(!exact_safety_roots.requires_source_fallback());
        assert_eq!(exact_safety_roots.roots(), &[5]);
        assert_eq!(exact_safety_roots.into_roots(), vec![5]);

        let source_fallback_roots =
            LightmountDependencyInvalidationContextRoots::new(true, vec![6]);
        assert!(source_fallback_roots.requires_source_fallback());
        assert_eq!(source_fallback_roots.roots(), &[6]);
    }

    #[test]
    fn lightmount_source_dependency_summary_and_batch_source_expose_typed_inputs() {
        let dependency_summary = lightmount_dependency_summary_for_selector(".active");
        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            true,
            lightmount_structural_boundary_summary_for_class("active"),
        );
        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            None,
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );

        assert!(source_summary
            .query_result(query.as_stylo_query())
            .has_any_dependency());
        assert!(source_summary
            .query_class(&Atom::from("active"))
            .has_any_dependency());
        assert!(source_summary.has_child_list_structural_dependency());
        assert!(source_summary.has_child_list_structural_dependency_for_requests(&[request]));
        assert!(!source_summary.has_relative_previous_sibling_dependency_for_requests(&[request]));
        assert!(!source_summary.has_slotted_dependency_for_requests(&[request]));
        assert!(source_summary.requires_empty_target_fallback_for_requests(&[request]));
        assert!(source_summary
            .structural_boundary_cleanup_roots_for_requests(&[request], &[9])
            .is_empty());

        let mut relative_dependency = LightmountDependencyQueryResult::default();
        relative_dependency.add_kind(LightmountDependencyKind::RelativePrevSibling);
        let mut relative_dependency_summary = LightmountDependencyInvalidationSummary::default();
        relative_dependency_summary
            .note_class_dependency(Atom::from("active"), relative_dependency);
        let relative_summary = LightmountSourceDependencySummary::new(
            relative_dependency_summary,
            false,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );
        let relative_request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            None,
            LightmountSourceDependencyRequestRequirement::relative_previous_sibling(),
        );
        assert!(relative_summary.requires_empty_target_fallback_for_requests(&[relative_request]));
        assert_eq!(
            relative_summary
                .structural_boundary_cleanup_roots_for_requests(&[relative_request], &[9]),
            vec![9]
        );

        let source_roots = [2_u32];
        let cause_roots = [3_u32];
        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &[],
        );
        assert!(source_summary
            .query_result(query.as_stylo_query())
            .has_any_dependency());
        assert_eq!(source.selected_fallback_roots(), &[2]);

        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &cause_roots,
        );
        assert_eq!(source.selected_fallback_roots(), &[3]);
    }

    #[test]
    fn lightmount_structural_empty_target_gate_requires_a_keyed_dependency() {
        let source_summary = LightmountSourceDependencySummary::new(
            lightmount_dependency_summary_for_selector("details > summary:first-of-type"),
            true,
            lightmount_structural_boundary_summary_for_type("details"),
        );
        let details_query =
            LightmountRetainedStyleInvalidationQuery::element_type(1_u32, "details".into());
        let details_request = LightmountSourceDependencyInvalidationRequest::new(
            &details_query,
            None,
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );
        assert!(
            source_summary.has_child_list_structural_dependency_for_requests(&[details_request])
        );

        let unrelated_query =
            LightmountRetainedStyleInvalidationQuery::element_type(2_u32, "em".into());
        let unrelated_request = LightmountSourceDependencyInvalidationRequest::new(
            &unrelated_query,
            None,
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );
        assert!(
            !source_summary.has_child_list_structural_dependency_for_requests(&[unrelated_request])
        );
        assert!(!source_summary.requires_empty_target_fallback_for_requests(&[unrelated_request,]));

        let universal_summary = LightmountSourceDependencySummary::new(
            lightmount_dependency_summary_for_selector(":first-child"),
            true,
            lightmount_universal_structural_boundary_summary(),
        );
        let universal_query = LightmountRetainedStyleInvalidationQuery::universal(3_u32);
        let universal_request = LightmountSourceDependencyInvalidationRequest::new(
            &universal_query,
            None,
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );
        assert!(universal_summary
            .has_child_list_structural_dependency_for_requests(&[universal_request]));
    }

    #[test]
    fn lightmount_source_dependency_summary_exposes_aggregate_predicates() {
        let mut dependency_summary = LightmountDependencyInvalidationSummary::default();

        let mut relative = LightmountDependencyQueryResult::default();
        relative.add_fallback_reason(LightmountDependencyFallbackReason::RelativeAnySelector);
        dependency_summary.note_class_dependency(Atom::from("anchor"), relative);

        let mut sibling = LightmountDependencyQueryResult::default();
        sibling.add_kind(LightmountDependencyKind::Siblings);
        dependency_summary.note_id_dependency(Atom::from("target"), sibling);

        let mut focus = LightmountDependencyQueryResult::default();
        focus.add_kind(LightmountDependencyKind::Element);
        dependency_summary
            .note_state_dependency(ElementState::FOCUS | ElementState::FOCUS_WITHIN, focus);

        let mut target = LightmountDependencyQueryResult::default();
        target.add_kind(LightmountDependencyKind::Element);
        dependency_summary.note_state_dependency(ElementState::URLTARGET, target);

        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            true,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );

        assert!(source_summary.has_relative_selector_dependency());
        assert!(source_summary.has_focus_dependency());
        assert!(source_summary.has_focus_within_dependency());
        assert!(source_summary.has_target_dependency());
        assert!(source_summary.has_child_list_structural_dependency());
        assert!(source_summary.has_sibling_dependency());
    }

    #[test]
    fn lightmount_child_list_retained_query_batch_drains_typed_parts() {
        let requirement = LightmountSourceDependencyRequestRequirement::child_list_structural();
        let query = LightmountRetainedStyleInvalidationQuery::universal(1_u32);
        let row = LightmountRetainedStyleChildListInvalidationQuery::new(query, requirement);
        let batch = LightmountRetainedStyleChildListInvalidationQueries::new(
            vec![row],
            vec![1],
            vec![2],
            vec![3],
        );
        let mut sink = ChildListInvalidationBatchSinkForTest::default();

        batch.drain_into(&mut sink);

        assert_eq!(sink.rows.len(), 1);
        assert_eq!(sink.rows[0].0.root(), 1);
        assert_eq!(
            sink.rows[0].1,
            LightmountSourceDependencyRequestRequirement::child_list_structural()
        );
        assert_eq!(sink.base_roots, vec![1]);
        assert_eq!(sink.empty_target_fallback_roots, vec![2]);
        assert_eq!(sink.relative_previous_sibling_cleanup_roots, vec![3]);
    }

    #[derive(Default)]
    struct ChildListInvalidationBatchSinkForTest {
        rows: Vec<(
            LightmountRetainedStyleInvalidationQuery<u32>,
            LightmountSourceDependencyRequestRequirement,
        )>,
        base_roots: Vec<u32>,
        empty_target_fallback_roots: Vec<u32>,
        relative_previous_sibling_cleanup_roots: Vec<u32>,
    }

    impl LightmountRetainedStyleChildListInvalidationQueriesSink<u32>
        for ChildListInvalidationBatchSinkForTest
    {
        fn record_child_list_retained_query(
            &mut self,
            query: LightmountRetainedStyleInvalidationQuery<u32>,
            requirement: LightmountSourceDependencyRequestRequirement,
        ) {
            self.rows.push((query, requirement));
        }

        fn extend_child_list_base_roots(&mut self, roots: Vec<u32>) {
            self.base_roots.extend(roots);
        }

        fn extend_child_list_empty_target_fallback_roots(&mut self, roots: Vec<u32>) {
            self.empty_target_fallback_roots.extend(roots);
        }

        fn extend_child_list_relative_previous_sibling_cleanup_roots(&mut self, roots: Vec<u32>) {
            self.relative_previous_sibling_cleanup_roots.extend(roots);
        }
    }

    #[test]
    fn lightmount_child_list_retained_query_batch_drains_into_sink() {
        let requirement = LightmountSourceDependencyRequestRequirement::relative_previous_sibling();
        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let row = LightmountRetainedStyleChildListInvalidationQuery::new(query, requirement);
        let batch = LightmountRetainedStyleChildListInvalidationQueries::new(
            vec![row],
            vec![1],
            vec![2],
            vec![3],
        );
        let mut sink = ChildListInvalidationBatchSinkForTest::default();

        batch.drain_into(&mut sink);

        assert_eq!(sink.rows.len(), 1);
        assert_eq!(sink.rows[0].0.root(), 1);
        assert_eq!(sink.rows[0].1, requirement);
        assert_eq!(sink.base_roots, vec![1]);
        assert_eq!(sink.empty_target_fallback_roots, vec![2]);
        assert_eq!(sink.relative_previous_sibling_cleanup_roots, vec![3]);
    }

    #[test]
    fn lightmount_child_list_retained_query_builder_merges_rows_and_roots() {
        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let mut builder = LightmountRetainedStyleChildListInvalidationQueryBuilder::new();
        builder.insert_queries(
            [query.clone()],
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );
        builder.insert_queries(
            [query],
            LightmountSourceDependencyRequestRequirement::exact(),
        );
        builder.insert_base_root(2);
        builder.insert_base_root(2);
        builder.insert_empty_target_fallback_root(3);
        builder.insert_empty_target_fallback_root(3);
        builder.insert_relative_previous_sibling_cleanup_root(4);
        builder.insert_relative_previous_sibling_cleanup_root(4);

        let batch = builder
            .into_queries()
            .expect("builder should emit query rows");
        let mut sink = ChildListInvalidationBatchSinkForTest::default();
        batch.drain_into(&mut sink);
        assert_eq!(sink.rows.len(), 1);
        assert_eq!(
            sink.rows[0].0.as_stylo_query(),
            LightmountStyleInvalidationQuery::Class("active")
        );
        assert_eq!(
            sink.rows[0].1,
            LightmountSourceDependencyRequestRequirement::exact()
        );
        assert_eq!(sink.base_roots, vec![2]);
        assert_eq!(sink.empty_target_fallback_roots, vec![3]);
        assert_eq!(sink.relative_previous_sibling_cleanup_roots, vec![4]);
        assert!(
            LightmountRetainedStyleChildListInvalidationQueryBuilder::<u32>::new()
                .into_queries()
                .is_none()
        );
    }

    #[test]
    fn lightmount_child_list_sibling_boundary_plan_classifies_cleanup_buckets() {
        fn flags(plan: &LightmountChildListSiblingBoundaryPlan<u32>) -> (bool, bool, bool) {
            (
                plan.includes_base_root(),
                plan.includes_empty_target_fallback_root(),
                plan.includes_relative_previous_sibling_cleanup_root(),
            )
        }

        let inserted_middle_previous = lightmount_child_list_sibling_boundary_plan(
            Some(1_u32),
            false,
            LightmountChildListSiblingBoundaryKind::AddedPreviousSibling {
                inserted_at_end: false,
            },
        )
        .expect("unchanged previous sibling should produce a plan");
        assert_eq!(*inserted_middle_previous.root(), 1);
        assert_eq!(flags(&inserted_middle_previous), (false, true, true));

        let inserted_end_previous = lightmount_child_list_sibling_boundary_plan(
            Some(2_u32),
            false,
            LightmountChildListSiblingBoundaryKind::AddedPreviousSibling {
                inserted_at_end: true,
            },
        )
        .expect("appended previous sibling should produce a plan");
        assert_eq!(flags(&inserted_end_previous), (true, true, true));

        let inserted_next = lightmount_child_list_sibling_boundary_plan(
            Some(3_u32),
            false,
            LightmountChildListSiblingBoundaryKind::AddedNextSibling,
        )
        .expect("unchanged next sibling should produce a plan");
        assert_eq!(flags(&inserted_next), (true, true, false));

        let removed_previous = lightmount_child_list_sibling_boundary_plan(
            Some(4_u32),
            false,
            LightmountChildListSiblingBoundaryKind::RemovedPreviousSibling,
        )
        .expect("unchanged previous sibling should produce a plan");
        assert_eq!(flags(&removed_previous), (true, true, true));

        let removed_next = lightmount_child_list_sibling_boundary_plan(
            Some(5_u32),
            false,
            LightmountChildListSiblingBoundaryKind::RemovedNextSibling,
        )
        .expect("unchanged next sibling should produce a plan");
        assert_eq!(flags(&removed_next), (true, true, false));

        let removed_earlier = lightmount_child_list_sibling_boundary_plan(
            Some(6_u32),
            false,
            LightmountChildListSiblingBoundaryKind::RemovedEarlierSibling,
        )
        .expect("unchanged earlier sibling should produce a plan");
        assert_eq!(flags(&removed_earlier), (false, false, true));

        assert!(lightmount_child_list_sibling_boundary_plan(
            Some(7_u32),
            true,
            LightmountChildListSiblingBoundaryKind::AddedNextSibling,
        )
        .is_none());
        assert!(lightmount_child_list_sibling_boundary_plan::<u32>(
            None,
            false,
            LightmountChildListSiblingBoundaryKind::RemovedNextSibling,
        )
        .is_none());

        let mut builder = LightmountRetainedStyleChildListInvalidationQueryBuilder::new();
        builder.insert_queries(
            [LightmountRetainedStyleInvalidationQuery::universal(10_u32)],
            LightmountSourceDependencyRequestRequirement::exact(),
        );
        inserted_end_previous.apply_to_builder(&mut builder);
        inserted_next.apply_to_builder(&mut builder);
        removed_earlier.apply_to_builder(&mut builder);

        let batch = builder
            .into_queries()
            .expect("query row keeps roots visible");
        let mut sink = ChildListInvalidationBatchSinkForTest::default();
        batch.drain_into(&mut sink);
        assert_eq!(sink.base_roots, vec![2, 3]);
        assert_eq!(sink.empty_target_fallback_roots, vec![2, 3]);
        assert_eq!(sink.relative_previous_sibling_cleanup_roots, vec![2, 6]);
    }

    #[test]
    fn lightmount_child_list_dependency_fallback_context_matches_query_root() {
        let removed_snapshot = LightmountElementDependencySnapshot::new(
            4_u32,
            "em".into(),
            ElementState::empty(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        );
        let added_nodes = [2_u32];
        let removed_nodes = [3_u32];
        let removed_snapshots = [removed_snapshot];
        let context = LightmountRetainedStyleChildListMutationContext::new(
            1,
            &added_nodes,
            &removed_nodes,
            &removed_snapshots,
            Some(5),
            Some(6),
        );

        let snapshot_query =
            LightmountRetainedStyleInvalidationQuery::element_type(4_u32, "em".into())
                .with_sibling_traversal(Some(LightmountRetainedStyleSiblingTraversal::new(
                    Some(7),
                    Some(8),
                )));
        let fallback =
            lightmount_child_list_dependency_fallback_context_for_query([context], &snapshot_query)
                .expect("removed snapshot root should match child-list context");
        assert_eq!(fallback.parent(), Some(1));
        assert_eq!(fallback.previous_sibling(), Some(7));
        assert_eq!(fallback.next_sibling(), Some(8));

        let added_query = LightmountRetainedStyleInvalidationQuery::universal(2_u32);
        let fallback =
            lightmount_child_list_dependency_fallback_context_for_query([context], &added_query)
                .expect("added root should match child-list context");
        assert_eq!(fallback.previous_sibling(), Some(5));
        assert_eq!(fallback.next_sibling(), Some(6));

        let unrelated_query = LightmountRetainedStyleInvalidationQuery::universal(9_u32);
        assert!(lightmount_child_list_dependency_fallback_context_for_query(
            [context],
            &unrelated_query,
        )
        .is_none());
    }

    #[test]
    fn lightmount_style_mutation_element_snapshot_preserves_first_old_values() {
        let mut first = LightmountStyleMutationElementSnapshot::default();
        first.record_attribute_change("class", Some("initial".into()));
        first.record_attribute_change("class", Some("middle".into()));
        assert_eq!(first.try_record_old_state(ElementState::CHECKED), Some(()));
        assert_eq!(first.try_record_old_state(ElementState::FOCUS), None);
        first.record_old_custom_states(vec!["first".into()]);
        first.record_old_custom_states(vec!["second".into()]);

        let mut second = LightmountStyleMutationElementSnapshot::default();
        second.record_attribute_change("class", Some("late".into()));
        second.record_attribute_change("id", Some("old-id".into()));
        second.record_attribute_change("data-state", None);
        second.try_record_old_state(ElementState::FOCUS);
        second.record_old_custom_states(vec!["incoming".into()]);

        first.merge_from(second);
        let changes = first.attribute_changes().collect::<Vec<_>>();

        assert_eq!(first.attribute_change_count(), 3);
        assert_eq!(changes[0].name(), "class");
        assert_eq!(changes[0].old_value(), Some("initial"));
        assert_eq!(changes[1].name(), "id");
        assert_eq!(changes[1].old_value(), Some("old-id"));
        assert_eq!(changes[2].name(), "data-state");
        assert_eq!(changes[2].old_value(), None);
        assert_eq!(first.old_state(), Some(ElementState::CHECKED));
        assert_eq!(
            first.old_custom_states(),
            Some(["first".to_string()].as_slice())
        );
    }

    #[derive(Default)]
    struct LightmountPlannedFallbackRootTargetPartsForTest {
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        fallback_roots: Vec<u32>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    }

    #[derive(Default)]
    struct LightmountPlannedSourceDependencyPartsForTest {
        source_index: Option<usize>,
        structural_boundary_cleanup_roots: Vec<u32>,
        target_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
        exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<u32>>,
        reasoned_fallback_roots: Vec<u32>,
        exact_safety_fallback_roots: Vec<u32>,
        fallback_roots: Vec<u32>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
    }

    impl LightmountPlannedFallbackRootInvalidationTargetPartsSink<u32>
        for LightmountPlannedFallbackRootTargetPartsForTest
    {
        fn set_planned_fallback_root_target_parts(
            &mut self,
            fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
            fallback_roots: Vec<u32>,
            fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        ) {
            self.fallback_kind = Some(fallback_kind);
            self.fallback_roots = fallback_roots;
            self.fallback_reasons = fallback_reasons;
        }
    }

    impl LightmountPlannedSourceDependencyInvalidationPartsSink<u32>
        for LightmountPlannedSourceDependencyPartsForTest
    {
        fn set_planned_source_dependency_source_index(&mut self, source_index: usize) {
            self.source_index = Some(source_index);
        }

        fn set_planned_source_dependency_structural_boundary_cleanup_roots(
            &mut self,
            structural_boundary_cleanup_roots: Vec<u32>,
        ) {
            self.structural_boundary_cleanup_roots = structural_boundary_cleanup_roots;
        }
    }

    impl LightmountPlannedSourceDependencyInvalidationTargetPartsSink<u32>
        for LightmountPlannedSourceDependencyPartsForTest
    {
        fn set_planned_retained_source_dependency_target_parts(
            &mut self,
            exact_queries: Vec<LightmountRetainedStyleInvalidationQuery<u32>>,
            fallback_kind: Option<LightmountRetainedSourceStyleInvalidationKind>,
            reasoned_fallback_roots: Vec<u32>,
            exact_safety_fallback_roots: Vec<u32>,
            fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        ) {
            self.target_kind = Some(LightmountRetainedSourceStyleInvalidationKind::RetainedQueries);
            self.fallback_kind = fallback_kind;
            self.exact_queries = exact_queries;
            self.reasoned_fallback_roots = reasoned_fallback_roots;
            self.exact_safety_fallback_roots = exact_safety_fallback_roots;
            self.fallback_reasons = fallback_reasons;
        }

        fn set_planned_fallback_source_dependency_target_parts(
            &mut self,
            fallback_kind: LightmountRetainedSourceStyleInvalidationKind,
            fallback_roots: Vec<u32>,
            fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        ) {
            self.target_kind = Some(fallback_kind);
            self.fallback_roots = fallback_roots;
            self.fallback_reasons = fallback_reasons;
        }

        fn set_planned_missing_fallback_roots_source_dependency_target_parts(
            &mut self,
            fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        ) {
            self.target_kind =
                Some(LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots);
            self.fallback_reasons = fallback_reasons;
        }
    }

    fn planned_source_dependency_parts_for_test(
        planned: LightmountPlannedSourceDependencyInvalidation<u32>,
    ) -> LightmountPlannedSourceDependencyPartsForTest {
        let mut sink = LightmountPlannedSourceDependencyPartsForTest::default();
        planned.drain_into(&mut sink);
        sink
    }

    fn planned_source_dependency_target_parts_for_test(
        target: LightmountPlannedSourceDependencyInvalidationTarget<u32>,
    ) -> LightmountPlannedSourceDependencyPartsForTest {
        let mut sink = LightmountPlannedSourceDependencyPartsForTest::default();
        target.drain_into(&mut sink);
        sink
    }

    #[derive(Default)]
    struct LightmountSourceDependencyBatchPlanForTest {
        work_sources: Vec<LightmountPlannedSourceDependencyInvalidation<u32>>,
        work_boundary_fallback: Option<LightmountPlannedFallbackRootInvalidationTarget<u32>>,
        requires_source_fallback: Option<LightmountPlannedSourceDependencyInvalidation<u32>>,
    }

    impl LightmountSourceDependencyInvalidationBatchPlanSink<u32>
        for LightmountSourceDependencyBatchPlanForTest
    {
        fn set_source_dependency_batch_work(
            &mut self,
            sources: Vec<LightmountPlannedSourceDependencyInvalidation<u32>>,
            boundary_fallback: Option<LightmountPlannedFallbackRootInvalidationTarget<u32>>,
        ) {
            self.work_sources = sources;
            self.work_boundary_fallback = boundary_fallback;
        }

        fn set_source_dependency_batch_requires_source_fallback(
            &mut self,
            source: LightmountPlannedSourceDependencyInvalidation<u32>,
        ) {
            self.requires_source_fallback = Some(source);
        }
    }

    fn source_dependency_batch_plan_for_test(
        plan: LightmountSourceDependencyInvalidationBatchPlan<u32>,
    ) -> LightmountSourceDependencyBatchPlanForTest {
        let mut sink = LightmountSourceDependencyBatchPlanForTest::default();
        plan.drain_into(&mut sink);
        sink
    }

    #[test]
    fn lightmount_planned_source_dependency_artifacts_drain_into_typed_sinks() {
        let empty_target_roots = [10_u32];
        let relative_cleanup_roots = [20_u32];
        let boundary_roots = LightmountSourceDependencyBoundaryRoots::new(
            &empty_target_roots,
            &relative_cleanup_roots,
        );
        assert_eq!(boundary_roots.empty_target_fallback_roots, &[10]);
        assert_eq!(
            boundary_roots.relative_previous_sibling_cleanup_roots,
            &[20]
        );

        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let planned =
            LightmountPlannedSourceDependencyInvalidation::retained_queries_with_fallback_kind(
                3,
                vec![query],
                Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback),
                vec![4],
                vec![5],
                [LightmountSourceInvalidationFallbackReason::FullSelector],
                vec![6],
            );
        let parts = planned_source_dependency_parts_for_test(planned);
        assert_eq!(parts.source_index, Some(3));
        assert_eq!(parts.structural_boundary_cleanup_roots, vec![6]);
        assert_eq!(
            parts.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::RetainedQueries)
        );
        assert_eq!(parts.exact_queries[0].root(), 1);
        assert_eq!(
            parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback)
        );
        assert_eq!(parts.reasoned_fallback_roots, vec![4]);
        assert_eq!(parts.exact_safety_fallback_roots, vec![5]);
        assert!(parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::FullSelector));

        let missing = planned_source_dependency_parts_for_test(
            LightmountPlannedSourceDependencyInvalidation::<u32>::missing_fallback_roots(
                7,
                [],
                Vec::new(),
            ),
        );
        assert_eq!(missing.source_index, Some(7));
        assert_eq!(
            missing.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots)
        );
        assert!(missing
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::MissingFallbackRoots));

        let boundary_fallback =
            LightmountPlannedFallbackRootInvalidationTarget::source_scope_fallback(vec![8], []);
        let mut fallback_parts = LightmountPlannedFallbackRootTargetPartsForTest::default();
        boundary_fallback.drain_into(&mut fallback_parts);
        assert_eq!(
            fallback_parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::SourceScopeFallback)
        );
        assert_eq!(fallback_parts.fallback_roots, vec![8]);
        assert!(fallback_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::SourceScopeFallback));

        let promoted_safety_target =
            LightmountPlannedSourceDependencyInvalidationTarget::from_source_dependency_work_parts(
                Vec::new(),
                None,
                Vec::new(),
                vec![9],
                [],
            )
            .expect("orphan exact-safety roots should become a fallback target");
        let promoted_safety_target =
            planned_source_dependency_target_parts_for_test(promoted_safety_target);
        assert_eq!(
            promoted_safety_target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly)
        );
        assert_eq!(promoted_safety_target.fallback_roots, vec![9]);
        assert!(promoted_safety_target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::InexactEmptyResult));

        let missing_selected_roots =
            LightmountPlannedSourceDependencyInvalidationTarget::<u32>::source_dependency_fallback(
                Vec::new(),
                [LightmountSourceInvalidationFallbackReason::FullSelector],
            );
        let missing_selected_roots =
            planned_source_dependency_target_parts_for_test(missing_selected_roots);
        assert_eq!(
            missing_selected_roots.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots)
        );
        assert!(missing_selected_roots
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::FullSelector));
        assert!(missing_selected_roots
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::MissingFallbackRoots));

        let source_plan = LightmountSourceDependencyInvalidationSourcePlan::work(Some(
            LightmountPlannedSourceDependencyInvalidationTarget::source_dependency_fallback(
                vec![11],
                [LightmountSourceInvalidationFallbackReason::FullSelector],
            ),
        ));
        let LightmountSourceDependencyInvalidationSourcePlan::Work { target } = source_plan else {
            panic!("source-local work plan should expose work target");
        };
        assert!(target.is_some());

        let source_plan =
            LightmountSourceDependencyInvalidationSourcePlan::requires_source_fallback(
                LightmountPlannedSourceDependencyInvalidationTarget::source_dependency_fallback(
                    Vec::<u32>::new(),
                    [LightmountSourceInvalidationFallbackReason::FullSelector],
                ),
            );
        let LightmountSourceDependencyInvalidationSourcePlan::RequiresSourceFallback { target } =
            source_plan
        else {
            panic!("source-local fallback plan should expose fallback target");
        };
        let target = planned_source_dependency_target_parts_for_test(target);
        assert_eq!(
            target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots)
        );
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::FullSelector));

        let batch_plan_sink = source_dependency_batch_plan_for_test(
            LightmountSourceDependencyInvalidationBatchPlan::work(
                vec![
                    LightmountPlannedSourceDependencyInvalidation::fallback_only(
                        1,
                        vec![2],
                        [],
                        Vec::new(),
                    ),
                ],
                None,
            ),
        );
        assert_eq!(batch_plan_sink.work_sources.len(), 1);
        assert!(batch_plan_sink.work_boundary_fallback.is_none());
        assert!(batch_plan_sink.requires_source_fallback.is_none());

        let batch_plan_sink = source_dependency_batch_plan_for_test(
            LightmountSourceDependencyInvalidationBatchPlan::requires_source_fallback(
                LightmountPlannedSourceDependencyInvalidation::missing_fallback_roots(
                    4,
                    [LightmountSourceInvalidationFallbackReason::FullSelector],
                    Vec::new(),
                ),
            ),
        );
        assert!(batch_plan_sink.work_sources.is_empty());
        assert!(batch_plan_sink.work_boundary_fallback.is_none());
        assert!(batch_plan_sink.requires_source_fallback.is_some());
    }

    #[test]
    fn lightmount_source_scope_and_fallback_roots_plans_label_targets() {
        let source_scope_target = lightmount_source_scope_fallback_plan(
            || vec![4_u32],
            [LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency],
        );
        let mut source_scope_parts = LightmountPlannedFallbackRootTargetPartsForTest::default();
        source_scope_target.drain_into(&mut source_scope_parts);
        assert_eq!(
            source_scope_parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::SourceScopeFallback)
        );
        assert_eq!(source_scope_parts.fallback_roots, vec![4]);
        assert!(source_scope_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::SourceScopeFallback));
        assert!(source_scope_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency));

        let fallback_target = lightmount_fallback_roots_plan(
            vec![5_u32],
            [LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency],
        );
        let mut fallback_parts = LightmountPlannedFallbackRootTargetPartsForTest::default();
        fallback_target.drain_into(&mut fallback_parts);
        assert_eq!(
            fallback_parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly)
        );
        assert_eq!(fallback_parts.fallback_roots, vec![5]);
        assert!(!fallback_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::SourceScopeFallback));
        assert!(fallback_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency));
    }

    #[test]
    fn lightmount_runtime_or_source_scope_fallback_plan_prefers_runtime_roots() {
        let source_scope_target = lightmount_runtime_or_source_scope_fallback_plan(
            Vec::new(),
            || vec![1_u32, 2],
            [LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency],
        );
        let mut source_scope_parts = LightmountPlannedFallbackRootTargetPartsForTest::default();
        source_scope_target.drain_into(&mut source_scope_parts);
        assert_eq!(
            source_scope_parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::SourceScopeFallback)
        );
        assert_eq!(source_scope_parts.fallback_roots, vec![1, 2]);
        assert!(source_scope_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::SourceScopeFallback));
        assert!(source_scope_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency));

        let runtime_target = lightmount_runtime_or_source_scope_fallback_plan(
            vec![3_u32],
            || panic!("source-scope roots should not be resolved when runtime roots exist"),
            [LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency],
        );
        let mut runtime_parts = LightmountPlannedFallbackRootTargetPartsForTest::default();
        runtime_target.drain_into(&mut runtime_parts);
        assert_eq!(
            runtime_parts.fallback_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly)
        );
        assert_eq!(runtime_parts.fallback_roots, vec![3]);
        assert!(!runtime_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::SourceScopeFallback));
        assert!(runtime_parts
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::UnsupportedStateDependency));
    }

    #[test]
    fn lightmount_stylesheet_source_scope_fallback_roots_dispatches_input() {
        struct Resolver;

        impl LightmountStylesheetSourceScopeFallbackRootsResolver<u32> for Resolver {
            fn stylesheet_owner_source_scope_fallback_roots(&self, owner: u32) -> Vec<u32> {
                assert_eq!(owner, 1);
                vec![10]
            }

            fn document_source_scope_fallback_roots(&self, document: u32) -> Vec<u32> {
                assert_eq!(document, 2);
                vec![20]
            }

            fn shadow_root_source_scope_fallback_roots(&self, root: u32) -> Vec<u32> {
                assert_eq!(root, 3);
                vec![30, 31]
            }
        }

        assert_eq!(
            lightmount_stylesheet_source_scope_fallback_roots(
                LightmountStylesheetSourceScopeFallbackInput::StylesheetOwner { owner: 1 },
                &Resolver,
            ),
            vec![10]
        );
        assert_eq!(
            lightmount_stylesheet_source_scope_fallback_roots(
                LightmountStylesheetSourceScopeFallbackInput::DocumentAdopted { document: 2 },
                &Resolver,
            ),
            vec![20]
        );
        assert_eq!(
            lightmount_stylesheet_source_scope_fallback_roots(
                LightmountStylesheetSourceScopeFallbackInput::ShadowRootAdopted { root: 3 },
                &Resolver,
            ),
            vec![30, 31]
        );
        assert!(lightmount_stylesheet_source_scope_fallback_roots(
            LightmountStylesheetSourceScopeFallbackInput::Unscoped,
            &Resolver,
        )
        .is_empty());
    }

    #[test]
    fn lightmount_source_dependency_batch_skips_unrelated_structural_source() {
        struct UnexpectedContextRootsProvider;

        impl LightmountSourceDependencyInvalidationContextRootsProvider<u32>
            for UnexpectedContextRootsProvider
        {
            fn context_roots_for_source_dependency(
                &mut self,
                _root: u32,
                _plan: LightmountDependencyContextRootPlan,
                _context: LightmountDependencyInvalidationFallbackContext<u32>,
            ) -> LightmountDependencyInvalidationContextRoots<u32> {
                panic!("an unrelated source query must not request context roots")
            }
        }

        let source_summary = LightmountSourceDependencySummary::new(
            lightmount_dependency_summary_for_selector("details > summary:first-of-type"),
            true,
            lightmount_structural_boundary_summary_for_type("details"),
        );
        let source_roots = [99_u32];
        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &[],
        );
        let query = LightmountRetainedStyleInvalidationQuery::element_type(1_u32, "em".into());
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            None,
            LightmountSourceDependencyRequestRequirement::child_list_structural(),
        );
        let empty_target_roots = [10_u32];
        let mut provider = UnexpectedContextRootsProvider;

        let plan = lightmount_source_dependency_invalidation_batch_plan(
            &[source],
            &[request],
            LightmountSourceDependencyBoundaryRoots::new(&empty_target_roots, &[]),
            &mut provider,
        );

        let plan = source_dependency_batch_plan_for_test(plan);
        assert!(plan.work_sources.is_empty());
        assert!(plan.work_boundary_fallback.is_none());
        assert!(plan.requires_source_fallback.is_none());
    }

    #[test]
    fn lightmount_source_dependency_batch_plan_uses_context_root_provider() {
        #[derive(Default)]
        struct ContextRootsProviderForTest {
            calls: usize,
        }

        impl LightmountSourceDependencyInvalidationContextRootsProvider<u32>
            for ContextRootsProviderForTest
        {
            fn context_roots_for_source_dependency(
                &mut self,
                root: u32,
                _plan: LightmountDependencyContextRootPlan,
                context: LightmountDependencyInvalidationFallbackContext<u32>,
            ) -> LightmountDependencyInvalidationContextRoots<u32> {
                self.calls += 1;
                assert_eq!(root, 1);
                assert_eq!(context.parent(), Some(2));
                assert_eq!(context.previous_sibling(), Some(3));
                assert_eq!(context.next_sibling(), Some(4));
                LightmountDependencyInvalidationContextRoots::new(false, vec![10])
            }
        }

        let mut dependency = LightmountDependencyQueryResult::default();
        dependency.add_fallback_reason(LightmountDependencyFallbackReason::NthOfDependency);
        let mut dependency_summary = LightmountDependencyInvalidationSummary::default();
        dependency_summary.note_class_dependency(Atom::from("active"), dependency);
        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            false,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );
        let source_roots = [99_u32];
        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &[],
        );
        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "active".into());
        let context = LightmountDependencyInvalidationFallbackContext::from_mutation_relation(
            Some(2),
            Some(3),
            Some(4),
        );
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            Some(context),
            LightmountSourceDependencyRequestRequirement::exact(),
        );
        let mut provider = ContextRootsProviderForTest::default();

        let plan = lightmount_source_dependency_invalidation_batch_plan(
            &[source],
            &[request],
            LightmountSourceDependencyBoundaryRoots::default(),
            &mut provider,
        );

        assert_eq!(provider.calls, 1);
        let mut plan = source_dependency_batch_plan_for_test(plan);
        assert!(plan.work_boundary_fallback.is_none());
        assert!(plan.requires_source_fallback.is_none());
        let sources = &mut plan.work_sources;
        assert_eq!(sources.len(), 1);
        let target = planned_source_dependency_parts_for_test(
            sources.pop().expect("source work should exist"),
        );
        assert_eq!(
            target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback)
        );
        assert_eq!(target.fallback_roots, vec![10]);
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::NthOfDependency));
    }

    #[test]
    fn lightmount_source_dependency_batch_plan_accumulates_missing_fallback_root_reasons() {
        struct ContextRootsProviderForTest;

        impl LightmountSourceDependencyInvalidationContextRootsProvider<u32>
            for ContextRootsProviderForTest
        {
            fn context_roots_for_source_dependency(
                &mut self,
                _root: u32,
                _plan: LightmountDependencyContextRootPlan,
                _context: LightmountDependencyInvalidationFallbackContext<u32>,
            ) -> LightmountDependencyInvalidationContextRoots<u32> {
                panic!("missing-root source fallback should not need context roots")
            }
        }

        let mut nth_dependency = LightmountDependencyQueryResult::default();
        nth_dependency.add_fallback_reason(LightmountDependencyFallbackReason::NthOfDependency);
        let mut full_dependency = LightmountDependencyQueryResult::default();
        full_dependency.add_fallback_reason(LightmountDependencyFallbackReason::FullSelector);
        let mut dependency_summary = LightmountDependencyInvalidationSummary::default();
        dependency_summary.note_class_dependency(Atom::from("nth"), nth_dependency);
        dependency_summary.note_class_dependency(Atom::from("full"), full_dependency);
        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            false,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );
        let source =
            LightmountSourceDependencyInvalidationBatchSource::new(&source_summary, &[], &[]);
        let nth_query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "nth".into());
        let full_query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "full".into());
        let requests = [
            LightmountSourceDependencyInvalidationRequest::new(
                &nth_query,
                None,
                LightmountSourceDependencyRequestRequirement::exact(),
            ),
            LightmountSourceDependencyInvalidationRequest::new(
                &full_query,
                None,
                LightmountSourceDependencyRequestRequirement::exact(),
            ),
        ];
        let mut provider = ContextRootsProviderForTest;

        let plan = lightmount_source_dependency_invalidation_batch_plan(
            &[source],
            &requests,
            LightmountSourceDependencyBoundaryRoots::default(),
            &mut provider,
        );

        let mut plan = source_dependency_batch_plan_for_test(plan);
        assert!(plan.work_sources.is_empty());
        assert!(plan.work_boundary_fallback.is_none());
        let target = planned_source_dependency_parts_for_test(
            plan.requires_source_fallback
                .take()
                .expect("missing fallback roots should force source fallback"),
        );
        assert_eq!(
            target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::MissingFallbackRoots)
        );
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::NthOfDependency));
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::FullSelector));
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::MissingFallbackRoots));
    }

    #[test]
    fn lightmount_source_dependency_batch_plan_uses_context_roots_for_custom_state_nth_of() {
        #[derive(Default)]
        struct ContextRootsProviderForTest {
            calls: usize,
        }

        impl LightmountSourceDependencyInvalidationContextRootsProvider<u32>
            for ContextRootsProviderForTest
        {
            fn context_roots_for_source_dependency(
                &mut self,
                root: u32,
                _plan: LightmountDependencyContextRootPlan,
                context: LightmountDependencyInvalidationFallbackContext<u32>,
            ) -> LightmountDependencyInvalidationContextRoots<u32> {
                self.calls += 1;
                assert_eq!(root, 1);
                assert_eq!(context.parent(), Some(2));
                assert_eq!(context.previous_sibling(), None);
                assert_eq!(context.next_sibling(), Some(3));
                LightmountDependencyInvalidationContextRoots::new(false, vec![3, 4])
            }
        }

        let mut dependency = LightmountDependencyQueryResult::default();
        dependency.add_kind(LightmountDependencyKind::Siblings);
        let mut dependency_summary = LightmountDependencyInvalidationSummary::default();
        dependency_summary.note_custom_state_dependency(AtomIdent::from("--active"), dependency);
        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            true,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );
        let source_roots = [99_u32];
        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &[],
        );
        let query =
            LightmountRetainedStyleInvalidationQuery::custom_state(1_u32, "--active".into());
        let context = LightmountDependencyInvalidationFallbackContext::from_mutation_relation(
            Some(2),
            None,
            Some(3),
        );
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            Some(context),
            LightmountSourceDependencyRequestRequirement::exact(),
        );
        let mut provider = ContextRootsProviderForTest::default();

        let plan = lightmount_source_dependency_invalidation_batch_plan(
            &[source],
            &[request],
            LightmountSourceDependencyBoundaryRoots::default(),
            &mut provider,
        );

        assert_eq!(provider.calls, 1);
        let mut plan = source_dependency_batch_plan_for_test(plan);
        assert!(plan.work_boundary_fallback.is_none());
        assert!(plan.requires_source_fallback.is_none());
        let sources = &mut plan.work_sources;
        assert_eq!(sources.len(), 1);
        let target = planned_source_dependency_parts_for_test(
            sources.pop().expect("source work should exist"),
        );
        assert_eq!(
            target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::ContextFallback)
        );
        assert_eq!(target.fallback_roots, vec![3, 4]);
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::NthOfDependency));
    }

    #[test]
    fn lightmount_source_dependency_batch_plan_keeps_scope_on_source_fallback() {
        #[derive(Default)]
        struct ContextRootsProviderForTest {
            calls: usize,
        }

        impl LightmountSourceDependencyInvalidationContextRootsProvider<u32>
            for ContextRootsProviderForTest
        {
            fn context_roots_for_source_dependency(
                &mut self,
                root: u32,
                _plan: LightmountDependencyContextRootPlan,
                context: LightmountDependencyInvalidationFallbackContext<u32>,
            ) -> LightmountDependencyInvalidationContextRoots<u32> {
                self.calls += 1;
                assert_eq!(root, 1);
                assert_eq!(context.parent(), Some(2));
                assert_eq!(context.previous_sibling(), Some(3));
                assert_eq!(context.next_sibling(), Some(4));
                LightmountDependencyInvalidationContextRoots::new(true, vec![10])
            }
        }

        let mut dependency = LightmountDependencyQueryResult::default();
        dependency.add_kind(LightmountDependencyKind::Scope);
        let mut dependency_summary = LightmountDependencyInvalidationSummary::default();
        dependency_summary.note_class_dependency(Atom::from("scoped"), dependency);
        let source_summary = LightmountSourceDependencySummary::new(
            dependency_summary,
            false,
            LightmountChildListStructuralBoundaryDependencySummary::default(),
        );
        let source_roots = [99_u32];
        let source = LightmountSourceDependencyInvalidationBatchSource::new(
            &source_summary,
            &source_roots,
            &[],
        );
        let query = LightmountRetainedStyleInvalidationQuery::class(1_u32, "scoped".into());
        let context = LightmountDependencyInvalidationFallbackContext::from_mutation_relation(
            Some(2),
            Some(3),
            Some(4),
        );
        let request = LightmountSourceDependencyInvalidationRequest::new(
            &query,
            Some(context),
            LightmountSourceDependencyRequestRequirement::exact(),
        );
        let mut provider = ContextRootsProviderForTest::default();

        let plan = lightmount_source_dependency_invalidation_batch_plan(
            &[source],
            &[request],
            LightmountSourceDependencyBoundaryRoots::default(),
            &mut provider,
        );

        assert_eq!(provider.calls, 1);
        let mut plan = source_dependency_batch_plan_for_test(plan);
        assert!(plan.work_boundary_fallback.is_none());
        assert!(plan.requires_source_fallback.is_none());
        let sources = &mut plan.work_sources;
        assert_eq!(sources.len(), 1);
        let target = planned_source_dependency_parts_for_test(
            sources.pop().expect("source work should exist"),
        );
        assert_eq!(
            target.target_kind,
            Some(LightmountRetainedSourceStyleInvalidationKind::FallbackOnly)
        );
        assert_eq!(target.fallback_roots, vec![99]);
        assert!(target
            .fallback_reasons
            .contains(&LightmountSourceInvalidationFallbackReason::ScopeDependency));
    }

    #[derive(Default)]
    struct LightmountSourceResultKindSummaryForTest {
        retained_source_unavailable_target_count: usize,
        source_scope_fallback_target_count: usize,
        context_fallback_target_count: usize,
    }

    impl LightmountSourceStyleInvalidationSourceResultKindSummarySink
        for LightmountSourceResultKindSummaryForTest
    {
        fn record_retained_source_unavailable_target(&mut self) {
            self.retained_source_unavailable_target_count += 1;
        }

        fn record_source_scope_fallback_target(&mut self) {
            self.source_scope_fallback_target_count += 1;
        }

        fn record_context_fallback_target(&mut self) {
            self.context_fallback_target_count += 1;
        }
    }

    #[test]
    fn lightmount_source_result_kind_records_summary_categories() {
        let mut summary = LightmountSourceResultKindSummaryForTest::default();

        LightmountSourceStyleInvalidationSourceResultKind::Exact.record_summary_into(&mut summary);
        LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedStyleSystem
            .record_summary_into(&mut summary);
        LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedCascadeData
            .record_summary_into(&mut summary);
        LightmountSourceStyleInvalidationSourceResultKind::SourceScopeFallback
            .record_summary_into(&mut summary);
        LightmountSourceStyleInvalidationSourceResultKind::ContextFallback
            .record_summary_into(&mut summary);

        assert_eq!(summary.retained_source_unavailable_target_count, 2);
        assert_eq!(summary.source_scope_fallback_target_count, 1);
        assert_eq!(summary.context_fallback_target_count, 1);
    }

    #[derive(Default)]
    struct LightmountFallbackRootAvailabilitySummaryForTest {
        missing_fallback_roots_target_count: usize,
    }

    impl LightmountSourceFallbackRootAvailabilitySummarySink
        for LightmountFallbackRootAvailabilitySummaryForTest
    {
        fn record_missing_fallback_roots_target(&mut self) {
            self.missing_fallback_roots_target_count += 1;
        }
    }

    #[test]
    fn lightmount_fallback_root_availability_records_missing_summary() {
        let mut summary = LightmountFallbackRootAvailabilitySummaryForTest::default();

        LightmountSourceFallbackRootAvailability::Available { root_count: 1 }
            .record_summary_into(&mut summary);
        LightmountSourceFallbackRootAvailability::Missing.record_summary_into(&mut summary);

        assert_eq!(summary.missing_fallback_roots_target_count, 1);
    }

    #[derive(Default)]
    struct LightmountSourceStyleInvalidationResultPartsForTest {
        affected_roots: Vec<u32>,
        fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
        fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
        fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        empty_result_is_exact: bool,
        matched_dependency_count: usize,
    }

    impl LightmountSourceStyleInvalidationResultSink<u32>
        for LightmountSourceStyleInvalidationResultPartsForTest
    {
        fn set_source_style_invalidation_result(
            &mut self,
            parts: LightmountSourceStyleInvalidationResultParts<u32>,
        ) {
            parts.drain_into(self);
        }
    }

    impl LightmountSourceStyleInvalidationResultPartsSink<u32>
        for LightmountSourceStyleInvalidationResultPartsForTest
    {
        fn set_source_style_invalidation_result_parts(
            &mut self,
            affected_roots: Vec<u32>,
            fallback_reasons: IndexSet<LightmountSourceInvalidationFallbackReason>,
            fallback_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
            fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
            empty_result_is_exact: bool,
            matched_dependency_count: usize,
        ) {
            self.affected_roots = affected_roots;
            self.fallback_reasons = fallback_reasons;
            self.fallback_kind = fallback_kind;
            self.fallback_root_availability = fallback_root_availability;
            self.empty_result_is_exact = empty_result_is_exact;
            self.matched_dependency_count = matched_dependency_count;
        }
    }

    fn source_style_invalidation_result_parts_for_test(
        result: LightmountSourceStyleInvalidationResult<u32>,
    ) -> LightmountSourceStyleInvalidationResultPartsForTest {
        let mut sink = LightmountSourceStyleInvalidationResultPartsForTest::default();
        result.drain_into(&mut sink);
        sink
    }

    #[test]
    fn lightmount_source_result_accumulator_reports_missing_fallback_roots() {
        let mut accumulated = LightmountSourceStyleInvalidationResultAccumulator::new();
        accumulated.merge_query_result(
            Vec::<u32>::new(),
            true,
            1,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector]),
        );

        let result = source_style_invalidation_result_parts_for_test(
            accumulated.into_source_result(&IndexSet::new()),
        );

        assert!(result.affected_roots.is_empty());
        assert_eq!(
            result.fallback_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::MissingFallbackRoots)
        );
        assert_eq!(
            result.fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Missing)
        );
        assert!(result.empty_result_is_exact);
        assert_eq!(result.matched_dependency_count, 1);
        assert_eq!(
            result.fallback_reasons,
            IndexSet::from([
                LightmountSourceInvalidationFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::MissingFallbackRoots,
            ])
        );
    }

    #[test]
    fn lightmount_query_result_merge_preserves_ordered_roots_and_reasons() {
        let first = LightmountSourceStyleInvalidationQueryResult::from_parts(
            vec![1, 2],
            true,
            2,
            [LightmountSourceInvalidationFallbackReason::FullSelector],
        );
        let second = LightmountSourceStyleInvalidationQueryResult::from_parts(
            vec![2, 3],
            false,
            1,
            [
                LightmountSourceInvalidationFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::RelativeAnySelector,
            ],
        );

        let merged = lightmount_merge_source_style_invalidation_query_results(first, second);
        let LightmountSourceStyleInvalidationQueryResult {
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
        } = merged;

        assert_eq!(affected_roots, vec![1, 2, 3]);
        assert!(!empty_result_is_exact);
        assert_eq!(matched_dependency_count, 3);
        assert_eq!(
            fallback_reasons,
            IndexSet::from([
                LightmountSourceInvalidationFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::RelativeAnySelector,
            ])
        );
    }

    #[test]
    fn lightmount_query_result_builder_preserves_roots_exactness_and_reasons() {
        let mut builder = LightmountSourceStyleInvalidationQueryResultBuilder::new();
        builder.note_affected_root(1);
        builder.note_affected_root(2);
        builder.note_affected_root(1);
        builder.note_empty_result_supported(true);
        builder.note_empty_result_supported(false);
        builder.note_fallback_reason(LightmountSourceInvalidationFallbackReason::FullSelector);
        builder.note_fallback_reason(LightmountSourceInvalidationFallbackReason::FullSelector);

        let result = builder.into_query_result(3);
        let LightmountSourceStyleInvalidationQueryResult {
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
        } = result;

        assert_eq!(affected_roots, vec![1, 2]);
        assert!(!empty_result_is_exact);
        assert_eq!(matched_dependency_count, 3);
        assert_eq!(
            fallback_reasons,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector])
        );
    }

    #[test]
    fn lightmount_query_result_drains_affected_roots() {
        let result = LightmountSourceStyleInvalidationQueryResult::from_parts(
            vec![1, 2, 1],
            true,
            2,
            [LightmountSourceInvalidationFallbackReason::FullSelector],
        );
        let mut roots = IndexSet::new();

        result.drain_affected_roots_into(&mut roots);

        assert_eq!(roots, IndexSet::from([1, 2]));
    }

    #[test]
    fn lightmount_snapshot_relative_roots_classify_empty_exactness() {
        let verified = LightmountSnapshotRelativeDependencyRoots::<u32>::new(Vec::new(), 2);

        assert!(verified.verified_all_dependencies(2, 2));
        assert!(verified.verified_all_collected_dependencies(2));
        assert!(verified.empty_result_is_exact(2, 2, false));
        assert!(!verified.empty_result_is_exact(3, 2, false));
        assert!(verified.empty_result_is_exact(0, 2, false));

        let rooted = LightmountSnapshotRelativeDependencyRoots::new(vec![1], 0);
        assert_eq!(rooted.roots(), &[1]);
        assert!(rooted.empty_result_is_exact(1, 2, true));
    }

    #[test]
    fn lightmount_normal_invalidation_dependency_plan_classifies_relative_filtering() {
        let no_snapshot_roots = LightmountSnapshotRelativeDependencyRoots::<u32>::default();

        let custom_state = lightmount_normal_style_invalidation_dependency_plan(
            LightmountStyleInvalidationQuery::CustomState("expanded"),
            1,
            1,
            &no_snapshot_roots,
        );
        assert!(custom_state.should_drop_relative_dependencies());
        assert!(custom_state.empty_result_is_exact());

        let verified_relative_roots =
            LightmountSnapshotRelativeDependencyRoots::<u32>::new(Vec::new(), 2);
        let mixed_dependencies = lightmount_normal_style_invalidation_dependency_plan(
            LightmountStyleInvalidationQuery::Class("active"),
            3,
            2,
            &verified_relative_roots,
        );
        assert!(mixed_dependencies.should_drop_relative_dependencies());
        assert!(!mixed_dependencies.empty_result_is_exact());

        let rooted_snapshot = LightmountSnapshotRelativeDependencyRoots::new(vec![1_u32], 1);
        let rooted_plan = lightmount_normal_style_invalidation_dependency_plan(
            LightmountStyleInvalidationQuery::Class("active"),
            1,
            1,
            &rooted_snapshot,
        );
        assert!(rooted_plan.should_drop_relative_dependencies());
        assert!(!rooted_plan.empty_result_is_exact());

        let unsupported_relative = lightmount_normal_style_invalidation_dependency_plan(
            LightmountStyleInvalidationQuery::Class("active"),
            1,
            1,
            &no_snapshot_roots,
        );
        assert!(!unsupported_relative.should_drop_relative_dependencies());
        assert!(!unsupported_relative.empty_result_is_exact());
    }

    #[test]
    fn lightmount_relative_invalidation_dependency_plan_classifies_empty_exactness() {
        let no_snapshot_roots = LightmountSnapshotRelativeDependencyRoots::<u32>::default();

        let no_dependencies =
            lightmount_relative_style_invalidation_dependency_plan(0, 0, false, &no_snapshot_roots);
        assert!(no_dependencies.empty_result_is_exact());

        let affected_roots =
            lightmount_relative_style_invalidation_dependency_plan(2, 2, true, &no_snapshot_roots);
        assert!(affected_roots.empty_result_is_exact());

        let verified_snapshot_dependencies =
            LightmountSnapshotRelativeDependencyRoots::<u32>::new(Vec::new(), 2);
        let verified = lightmount_relative_style_invalidation_dependency_plan(
            2,
            2,
            false,
            &verified_snapshot_dependencies,
        );
        assert!(verified.empty_result_is_exact());

        let unsupported_relative = lightmount_relative_style_invalidation_dependency_plan(
            2,
            1,
            false,
            &verified_snapshot_dependencies,
        );
        assert!(!unsupported_relative.empty_result_is_exact());
    }

    #[test]
    fn lightmount_relative_invalidation_query_result_merges_direct_and_snapshot_roots() {
        let snapshot_roots = LightmountSnapshotRelativeDependencyRoots::new(vec![2_u32, 3], 1);

        let result =
            lightmount_relative_style_invalidation_query_result(vec![1, 2], &snapshot_roots, 2, 1);
        let LightmountSourceStyleInvalidationQueryResult {
            affected_roots,
            empty_result_is_exact,
            matched_dependency_count,
            fallback_reasons,
        } = result;

        assert_eq!(affected_roots, vec![1, 2, 3]);
        assert!(empty_result_is_exact);
        assert_eq!(matched_dependency_count, 2);
        assert!(fallback_reasons.is_empty());

        let no_snapshot_roots = LightmountSnapshotRelativeDependencyRoots::<u32>::default();
        let unsupported_empty =
            lightmount_relative_style_invalidation_query_result([], &no_snapshot_roots, 2, 1);
        assert!(!unsupported_empty.empty_result_is_exact);

        let no_dependencies =
            lightmount_relative_style_invalidation_query_result([], &no_snapshot_roots, 0, 0);
        assert!(no_dependencies.empty_result_is_exact);
    }

    #[test]
    fn lightmount_source_result_accumulator_consumes_typed_query_result() {
        let mut accumulated = LightmountSourceStyleInvalidationResultAccumulator::new();
        accumulated.merge_invalidation_query_result(
            LightmountSourceStyleInvalidationQueryResult::from_parts(
                vec![1],
                true,
                1,
                [LightmountSourceInvalidationFallbackReason::FullSelector],
            ),
        );

        let result = source_style_invalidation_result_parts_for_test(
            accumulated.into_source_result(&IndexSet::from([2])),
        );

        assert_eq!(result.affected_roots, vec![2]);
        assert_eq!(
            result.fallback_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::Fallback)
        );
        assert_eq!(
            result.fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 1 })
        );
        assert!(result.empty_result_is_exact);
        assert_eq!(result.matched_dependency_count, 1);
        assert_eq!(
            result.fallback_reasons,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector])
        );
    }

    #[test]
    fn lightmount_source_result_accumulator_uses_exact_safety_roots_for_fallback() {
        let mut accumulated = LightmountSourceStyleInvalidationResultAccumulator::new();
        accumulated.merge_query_result(
            vec![1],
            true,
            1,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector]),
        );

        let result = source_style_invalidation_result_parts_for_test(
            accumulated.into_source_result(&IndexSet::from([2])),
        );

        assert_eq!(result.affected_roots, vec![2]);
        assert_eq!(
            result.fallback_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::Fallback)
        );
        assert_eq!(
            result.fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 1 })
        );
        assert_eq!(
            result.fallback_reasons,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector])
        );
    }

    #[test]
    fn lightmount_source_result_accumulator_converts_empty_inexact_result_to_reason() {
        let mut accumulated = LightmountSourceStyleInvalidationResultAccumulator::new();
        accumulated.merge_query_result(Vec::<u32>::new(), true, 0, IndexSet::new());

        let result = source_style_invalidation_result_parts_for_test(
            accumulated.into_source_result(&IndexSet::from([1])),
        );

        assert_eq!(result.affected_roots, vec![1]);
        assert_eq!(
            result.fallback_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::Fallback)
        );
        assert_eq!(
            result.fallback_reasons,
            IndexSet::from([LightmountSourceInvalidationFallbackReason::InexactEmptyResult])
        );
    }

    #[derive(Default)]
    struct LightmountSourceResultDrainForTest {
        source_result_count: Option<usize>,
        source_index: Option<usize>,
        exact_roots: Vec<u32>,
        source_fallback_roots: Vec<u32>,
        diagnostic_kind: Option<LightmountSourceStyleInvalidationSourceResultKind>,
        diagnostic_fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        diagnostic_fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
        cleanup_clear_all_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
        cleanup_includes_fallback_context_for_clear_all: bool,
    }

    impl LightmountInvalidationSourceResultsSink<u32> for LightmountSourceResultDrainForTest {
        fn record_lightmount_invalidation_source_result_count(&mut self, count: usize) {
            self.source_result_count = Some(count);
        }

        fn record_lightmount_invalidation_source_result(
            &mut self,
            result: LightmountSourceStyleInvalidationSourceResult<u32>,
        ) {
            result.drain_into(self);
        }
    }

    impl LightmountSourceStyleInvalidationSourceResultSink<u32> for LightmountSourceResultDrainForTest {
        fn record_source_style_invalidation_source_result(
            &mut self,
            parts: LightmountSourceStyleInvalidationSourceResultParts<u32>,
        ) {
            parts.drain_into(self);
        }
    }

    impl LightmountSourceStyleInvalidationSourceResultPartsSink<u32>
        for LightmountSourceResultDrainForTest
    {
        fn record_source_style_invalidation_source_result_parts(
            &mut self,
            source_index: usize,
            affected_roots: LightmountSourceAffectedRootsCleanup<u32>,
            target_result_record: LightmountSourceStyleInvalidationTargetResultRecord,
        ) {
            self.source_index = Some(source_index);
            affected_roots.drain_into(self);
            if let Some(diagnostic_facts) = target_result_record.drain_cleanup_into(self) {
                diagnostic_facts.drain_into(self);
            }
        }
    }

    impl LightmountSourceAffectedRootsCleanupSink<u32> for LightmountSourceResultDrainForTest {
        fn extend_exact_affected_roots(&mut self, roots: &[u32]) {
            self.exact_roots.extend(roots.iter().copied());
        }

        fn extend_source_fallback_roots(&mut self, roots: &[u32]) {
            self.source_fallback_roots.extend(roots.iter().copied());
        }
    }

    impl LightmountSourceStyleInvalidationTargetResultCleanupFactsSink
        for LightmountSourceResultDrainForTest
    {
        fn set_source_style_invalidation_target_result_cleanup_facts(
            &mut self,
            facts: LightmountSourceStyleInvalidationTargetResultCleanupFacts,
        ) {
            facts.drain_parts_into(self);
        }
    }

    impl LightmountSourceStyleInvalidationTargetResultCleanupFactsPartsSink
        for LightmountSourceResultDrainForTest
    {
        fn set_source_style_invalidation_target_result_cleanup_fact_parts(
            &mut self,
            _fallback_context_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
            clear_all_cleanup_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
            include_fallback_context_for_clear_all: bool,
            _requires_fallback_handling: bool,
        ) {
            self.cleanup_clear_all_reasons = clear_all_cleanup_reasons;
            self.cleanup_includes_fallback_context_for_clear_all =
                include_fallback_context_for_clear_all;
        }
    }

    impl LightmountSourceStyleInvalidationTargetResultDiagnosticFactsSink
        for LightmountSourceResultDrainForTest
    {
        fn set_source_style_invalidation_target_result_diagnostic_facts(
            &mut self,
            facts: LightmountSourceStyleInvalidationTargetResultDiagnosticFacts,
        ) {
            facts.drain_parts_into(self);
        }
    }

    impl LightmountSourceStyleInvalidationTargetResultDiagnosticFactsPartsSink
        for LightmountSourceResultDrainForTest
    {
        fn set_source_style_invalidation_target_result_diagnostic_fact_parts(
            &mut self,
            kind: LightmountSourceStyleInvalidationSourceResultKind,
            _exact: bool,
            _empty_result_is_exact: bool,
            _matched_dependency_count: usize,
            fallback_reasons: Vec<LightmountSourceInvalidationFallbackReason>,
            fallback_root_availability: Option<LightmountSourceFallbackRootAvailability>,
            _affected_root_count: usize,
        ) {
            self.diagnostic_kind = Some(kind);
            self.diagnostic_fallback_reasons = fallback_reasons;
            self.diagnostic_fallback_root_availability = fallback_root_availability;
        }
    }

    #[test]
    fn lightmount_source_result_drains_unavailable_retained_policy() {
        let result = LightmountSourceStyleInvalidationSourceResult::unavailable_retained_source(
            3,
            LightmountSourceInvalidationFallbackReason::MissingRetainedCascadeData,
            &IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector]),
            &IndexSet::from([1]),
            &IndexSet::from([2]),
        );
        let mut sink = LightmountSourceResultDrainForTest::default();

        result.drain_into(&mut sink);

        assert_eq!(sink.source_index, Some(3));
        assert!(sink.exact_roots.is_empty());
        assert_eq!(sink.source_fallback_roots, vec![1, 2]);
        assert_eq!(
            sink.diagnostic_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedCascadeData)
        );
        assert_eq!(
            sink.diagnostic_fallback_reasons,
            vec![
                LightmountSourceInvalidationFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::MissingRetainedCascadeData,
            ]
        );
        assert_eq!(
            sink.diagnostic_fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 2 })
        );
        assert!(sink.cleanup_clear_all_reasons.is_empty());
        assert!(sink.cleanup_includes_fallback_context_for_clear_all);
    }

    #[test]
    fn lightmount_source_result_drains_missing_roots_clear_all_policy() {
        let result = LightmountSourceStyleInvalidationSourceResult::fallback(
            0,
            LightmountSourceStyleInvalidationSourceResultKind::MissingFallbackRoots,
            false,
            1,
            vec![LightmountSourceInvalidationFallbackReason::FullSelector],
            Some(LightmountSourceFallbackRootAvailability::Missing),
            Vec::<u32>::new(),
        );
        let mut sink = LightmountSourceResultDrainForTest::default();

        result.drain_into(&mut sink);

        assert_eq!(
            sink.cleanup_clear_all_reasons,
            vec![
                LightmountSourceInvalidationFallbackReason::FullSelector,
                LightmountSourceInvalidationFallbackReason::MissingFallbackRoots,
            ]
        );
    }

    #[test]
    fn lightmount_invalidation_result_drains_source_result_table() {
        let result = LightmountInvalidationResult::from_source_results(vec![
            LightmountSourceStyleInvalidationSourceResult::exact_result(0, vec![7], true, 1),
        ]);
        let mut sink = LightmountSourceResultDrainForTest::default();

        result.drain_source_results_into(&mut sink);

        assert_eq!(sink.source_result_count, Some(1));
        assert_eq!(sink.source_index, Some(0));
        assert_eq!(sink.exact_roots, vec![7]);
        assert!(sink.source_fallback_roots.is_empty());
    }

    #[test]
    fn lightmount_invalidation_result_builder_builds_source_result_table() {
        let mut builder = LightmountInvalidationResultBuilder::new();
        builder.push_missing_retained_style_system_source(
            2,
            &IndexSet::from([LightmountSourceInvalidationFallbackReason::FullSelector]),
            &IndexSet::from([3]),
            &IndexSet::from([4]),
        );
        let result = builder.finish();
        let mut sink = LightmountSourceResultDrainForTest::default();

        result.drain_source_results_into(&mut sink);

        assert_eq!(sink.source_result_count, Some(1));
        assert_eq!(sink.source_index, Some(2));
        assert_eq!(sink.source_fallback_roots, vec![3, 4]);
        assert_eq!(
            sink.diagnostic_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedStyleSystem)
        );
        assert_eq!(
            sink.diagnostic_fallback_root_availability,
            Some(LightmountSourceFallbackRootAvailability::Available { root_count: 2 })
        );

        let mut builder = LightmountInvalidationResultBuilder::new();
        builder.push_missing_retained_cascade_data_source(
            5,
            &IndexSet::new(),
            &IndexSet::new(),
            &IndexSet::from([6]),
        );
        let result = builder.finish();
        let mut sink = LightmountSourceResultDrainForTest::default();

        result.drain_source_results_into(&mut sink);

        assert_eq!(sink.source_result_count, Some(1));
        assert_eq!(sink.source_index, Some(5));
        assert_eq!(sink.source_fallback_roots, vec![6]);
        assert_eq!(
            sink.diagnostic_kind,
            Some(LightmountSourceStyleInvalidationSourceResultKind::MissingRetainedCascadeData)
        );
    }

    #[test]
    fn lightmount_dependency_processor_support_rejects_unsupported_shapes() {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selector = SelectorParser::parse_author_origin_no_namespace(".subject", &url_data)
            .expect("selector should parse")
            .slice()[0]
            .clone();
        let dependency_for_kind = |kind| Dependency::new(selector.clone(), 0, None, kind);

        let normal = dependency_for_kind(DependencyInvalidationKind::Normal(
            NormalDependencyInvalidationKind::ElementAndDescendants,
        ));
        assert!(lightmount_dependency_supported_by_retained_processor(
            &normal
        ));
        assert!(lightmount_dependency_empty_result_supported_by_retained_processor(&normal));

        let scope = dependency_for_kind(DependencyInvalidationKind::Scope(
            ScopeDependencyInvalidationKind::ScopeEnd,
        ));
        assert!(lightmount_dependency_supported_by_retained_processor(
            &scope
        ));
        assert!(lightmount_dependency_empty_result_supported_by_retained_processor(&scope));

        let full = dependency_for_kind(DependencyInvalidationKind::FullSelector);
        assert!(!lightmount_dependency_supported_by_retained_processor(
            &full
        ));
        assert!(!lightmount_dependency_empty_result_supported_by_retained_processor(&full));

        let relative = dependency_for_kind(DependencyInvalidationKind::Relative(
            RelativeDependencyInvalidationKind::Ancestors,
        ));
        assert!(!lightmount_dependency_supported_by_retained_processor(
            &relative
        ));
        assert!(!lightmount_dependency_empty_result_supported_by_retained_processor(&relative));

        let full_next = ThinArc::from_header_and_iter(
            (),
            [dependency_for_kind(
                DependencyInvalidationKind::FullSelector,
            )]
            .into_iter(),
        );
        let normal_with_unsupported_next = Dependency::new(
            selector,
            0,
            Some(full_next),
            DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Element),
        );
        assert!(!lightmount_dependency_supported_by_retained_processor(
            &normal_with_unsupported_next
        ));
        assert!(
            !lightmount_dependency_empty_result_supported_by_retained_processor(
                &normal_with_unsupported_next
            )
        );
    }

    #[test]
    fn lightmount_retained_processor_dependency_effect_classifies_dependency() {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selector = SelectorParser::parse_author_origin_no_namespace(".subject", &url_data)
            .expect("selector should parse")
            .slice()[0]
            .clone();
        let dependency_for_kind = |kind| Dependency::new(selector.clone(), 0, None, kind);

        let normal = dependency_for_kind(DependencyInvalidationKind::Normal(
            NormalDependencyInvalidationKind::Element,
        ));
        assert_eq!(
            lightmount_retained_processor_dependency_effect(&normal),
            LightmountRetainedProcessorDependencyEffect::Retained {
                empty_result_is_exact: true
            }
        );

        let full = dependency_for_kind(DependencyInvalidationKind::FullSelector);
        assert_eq!(
            lightmount_retained_processor_dependency_effect(&full),
            LightmountRetainedProcessorDependencyEffect::Fallback(
                LightmountSourceInvalidationFallbackReason::FullSelector
            )
        );
    }

    #[test]
    fn lightmount_relative_summary_does_not_treat_used_flag_as_unknown_dependency() {
        let mut map = AdditionalRelativeSelectorInvalidationMap::new();
        map.used = true;

        let summary = lightmount_dependency_summary_for_relative_invalidation_map(&map);

        assert!(!summary.has_unknown_dependency());
        assert!(!summary.query_universal().has_any_dependency());
    }

    #[test]
    fn lightmount_relative_summary_keeps_ancestor_traversal_unknown() {
        let mut map = AdditionalRelativeSelectorInvalidationMap::new();
        map.needs_ancestors_traversal = true;

        let summary = lightmount_dependency_summary_for_relative_invalidation_map(&map);

        assert!(summary.has_unknown_dependency());
    }

    #[test]
    fn lightmount_nth_of_dependencies_are_sibling_sensitive_by_key() {
        let mut summary = LightmountDependencyInvalidationSummary::default();
        let class = Atom::from("c");
        let other_class = Atom::from("other");
        let id = Atom::from("target");
        let attribute = LocalName::from("data-active");

        summary.note_nth_of_class_dependency(class.clone());
        summary.note_nth_of_id_dependency(id.clone());
        summary.note_nth_of_attribute_dependency(attribute.clone());
        summary.note_nth_of_state_dependency(ElementState::FOCUS);
        summary.note_nth_of_state_dependency(ElementState::empty());

        assert!(!summary.has_unknown_dependency());
        let class_result = summary.query_class(&class);
        assert!(class_result.requires_fallback());
        assert_eq!(class_result.kinds(), &[LightmountDependencyKind::Siblings]);
        assert_eq!(
            class_result.fallback_reasons(),
            &[LightmountDependencyFallbackReason::NthOfDependency]
        );
        assert_eq!(
            class_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );
        assert!(!summary.query_class(&other_class).has_any_dependency());
        assert_eq!(
            summary.query_id(&id).kinds(),
            &[LightmountDependencyKind::Siblings]
        );
        assert_eq!(
            summary.query_attribute(&attribute).kinds(),
            &[LightmountDependencyKind::Siblings]
        );
        assert_eq!(
            summary.query_focus().kinds(),
            &[LightmountDependencyKind::Siblings]
        );
    }

    #[test]
    fn lightmount_summary_marks_nested_relative_selector_lists_for_fallback() {
        let summary = lightmount_dependency_summary_for_selector(
            "#target:has(:is(.item + .item + .item > .child + .child + .child))",
        );

        let item_result = summary.query_class(&Atom::from("item"));
        assert!(item_result.has_any_dependency());
        assert!(item_result.requires_fallback());
        assert!(item_result
            .fallback_reasons()
            .contains(&LightmountDependencyFallbackReason::NestedRelativeSelectorDependency));
        assert_eq!(
            item_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );

        let child_result = summary.query_class(&Atom::from("child"));
        assert!(child_result.has_any_dependency());
        assert!(child_result.requires_fallback());
        assert!(child_result
            .fallback_reasons()
            .contains(&LightmountDependencyFallbackReason::NestedRelativeSelectorDependency));
        assert_eq!(
            child_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );
    }

    #[test]
    fn lightmount_summary_exposes_link_pseudos_as_href_attribute_dependencies() {
        let summary = lightmount_dependency_summary_for_selector("#target:has(:any-link)");
        let href = LocalName::from("href");

        assert!(summary.query_attribute(&href).has_any_dependency());
        assert!(!summary
            .query_attribute(&LocalName::from("class"))
            .has_any_dependency());
    }
}

fn lightmount_selector_map_has_sibling_dependency<T: 'static>(
    map: &SelectorMap<T>,
    dependency: impl Fn(&T) -> &Dependency + Copy,
) -> bool {
    map.root
        .iter()
        .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        || map.id_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.class_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.attribute_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.local_name_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.namespace_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map
            .rare_pseudo_classes
            .iter()
            .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        || map
            .other
            .iter()
            .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
}

fn lightmount_dependency_is_sibling_sensitive(dependency: &Dependency) -> bool {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Siblings) => true,
        DependencyInvalidationKind::Relative(
            RelativeDependencyInvalidationKind::PrevSibling
            | RelativeDependencyInvalidationKind::AncestorPrevSibling
            | RelativeDependencyInvalidationKind::EarlierSibling
            | RelativeDependencyInvalidationKind::AncestorEarlierSibling,
        ) => true,
        _ => {
            dependency.right_combinator_is_next_sibling()
                || dependency.dependency_is_relative_with_single_next_sibling()
        },
    }
}
