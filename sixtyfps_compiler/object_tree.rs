/*!
 This module contains the intermediate representation of the code in the form of an object tree
*/

use crate::diagnostics::Diagnostics;
use crate::expression_tree::Expression;
use crate::parser::{syntax_nodes, Spanned, SyntaxKind, SyntaxNodeEx};
use crate::typeregister::{Type, TypeRegister};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
/// The full document (a complete file)
#[derive(Default, Debug)]
pub struct Document {
    //     node: SyntaxNode,
    pub inner_components: Vec<Rc<Component>>,
    pub root_component: Rc<Component>,
}

impl Document {
    pub fn from_node(
        node: syntax_nodes::Document,
        diag: &mut Diagnostics,
        tr: &mut TypeRegister,
    ) -> Self {
        debug_assert_eq!(node.kind(), SyntaxKind::Document);
        let inner_components = node
            .Component()
            .map(|n| {
                let compo = Component::from_node(n, diag, tr);
                tr.add(compo.clone());
                compo
            })
            .collect::<Vec<_>>();

        Document {
            // FIXME: one should use the `component` hint instead of always returning the last
            root_component: inner_components.last().cloned().unwrap_or_default(),

            inner_components,
        }
    }
}

/// A component is a type in the language which can be instantiated,
/// Or is materialized for repeated expression.
#[derive(Default, Debug)]
pub struct Component {
    //     node: SyntaxNode,
    pub id: String,
    pub root_element: ElementRc,

    /// The parent element within the parent component if this component represents a repeated element
    pub parent_element: Weak<RefCell<Element>>,

    /// List of elements that are not attached to the root anymore because they have been
    /// optimized away, but their properties may still be in use
    pub optimized_elements: RefCell<Vec<ElementRc>>,

    /// Map of resources to embed in the generated binary, indexed by their absolute path on
    /// disk on the build system and valued by a unique integer id, that can be used by the
    /// generator for symbol generation.
    pub embedded_file_resources: RefCell<HashMap<String, usize>>,

    /// LayoutConstraints
    pub layout_constraints: RefCell<crate::layout::LayoutConstraints>,
}

impl Component {
    pub fn from_node(
        node: syntax_nodes::Component,
        diag: &mut Diagnostics,
        tr: &TypeRegister,
    ) -> Rc<Self> {
        let c = Rc::new(Component {
            id: node.child_text(SyntaxKind::Identifier).unwrap_or_default(),
            root_element: Rc::new(RefCell::new(Element::from_node(
                node.Element(),
                "root".into(),
                diag,
                tr,
            ))),
            ..Default::default()
        });
        let weak = Rc::downgrade(&c);
        recurse_elem(&c.root_element, &(), &mut |e, _| {
            e.borrow_mut().enclosing_component = weak.clone()
        });
        c
    }
}

#[derive(Clone, Debug, Default)]
pub struct PropertyDeclaration {
    pub property_type: Type,
    pub type_location: crate::diagnostics::Span,
    pub expose_in_public_api: bool,
}

/// An Element is an instentation of a Component
#[derive(Default, Debug)]
pub struct Element {
    /// The id as named in the original .60 file.
    ///
    /// Note that it can only be used for lookup before inlining.
    /// After inlining there can be duplicated id in the component.
    /// The id are then re-assigned unique id in the assign_id pass
    pub id: String,
    //pub base: QualifiedTypeName,
    pub base_type: crate::typeregister::Type,
    /// Currently contains also the signals. FIXME: should that be changed?
    pub bindings: HashMap<String, Expression>,
    pub children: Vec<ElementRc>,
    /// The component which contains this element.
    pub enclosing_component: Weak<Component>,

    pub property_declarations: HashMap<String, PropertyDeclaration>,

    pub property_animations: HashMap<String, ElementRc>,

    /// Tis element is part of a `for <xxx> in <model>:
    pub repeated: Option<RepeatedElementInfo>,

    /// The AST node, if available
    pub node: Option<syntax_nodes::Element>,
}

#[derive(Debug, Clone)]
/// If the parent element is a repeated element, this has information about the models
pub struct RepeatedElementInfo {
    pub model: Expression,
    pub model_data_id: String,
    pub index_id: String,
}

pub type ElementRc = Rc<RefCell<Element>>;

impl Element {
    pub fn from_node(
        node: syntax_nodes::Element,
        id: String,
        diag: &mut Diagnostics,
        tr: &TypeRegister,
    ) -> Self {
        let base = QualifiedTypeName::from_node(node.QualifiedName());
        let mut r = Element {
            id,
            base_type: tr.lookup(&base.to_string()),
            node: Some(node.clone()),
            ..Default::default()
        };
        if !r.base_type.is_object_type() {
            diag.push_error(format!("Unknown type {}", base), node.QualifiedName().span());
            return r;
        }

        for prop_decl in node.PropertyDeclaration() {
            let qualified_type_node = prop_decl.QualifiedName();
            let type_span = qualified_type_node.span();
            let qualified_type = QualifiedTypeName::from_node(qualified_type_node);

            let prop_type = tr.lookup_qualified(&qualified_type.members);

            match prop_type {
                Type::Invalid => {
                    diag.push_error(
                        format!("Unknown property type '{}'", qualified_type.to_string()),
                        type_span.clone(),
                    );
                }
                _ => (),
            };

            let prop_name_token =
                prop_decl.DeclaredIdentifier().child_token(SyntaxKind::Identifier).unwrap();

            let prop_name = prop_name_token.text().to_string();
            if !matches!(r.lookup_property(&prop_name), Type::Invalid) {
                diag.push_error(
                    format!("Cannot override property '{}'", prop_name),
                    prop_name_token.span(),
                )
            }

            r.property_declarations.insert(
                prop_name.clone(),
                PropertyDeclaration {
                    property_type: prop_type,
                    type_location: type_span,
                    ..Default::default()
                },
            );

            if let Some(csn) = prop_decl.BindingExpression() {
                if r.bindings.insert(prop_name, Expression::Uncompiled(csn.into())).is_some() {
                    diag.push_error("Duplicated property binding".into(), prop_name_token.span());
                }
            }
        }

        r.parse_bindings(&base, node.Binding(), diag);

        for sig_decl in node.SignalDeclaration() {
            let name_token =
                sig_decl.DeclaredIdentifier().child_token(SyntaxKind::Identifier).unwrap();
            let name = name_token.text().to_string();
            r.property_declarations.insert(
                name,
                PropertyDeclaration {
                    property_type: Type::Signal,
                    type_location: sig_decl.span(),
                    ..Default::default()
                },
            );
        }

        for con_node in node.SignalConnection() {
            let name_token = match con_node.child_token(SyntaxKind::Identifier) {
                Some(x) => x,
                None => continue,
            };
            let name = name_token.text().to_string();
            let prop_type = r.lookup_property(&name);
            if !matches!(prop_type, Type::Signal) {
                diag.push_error(
                    format!("'{}' is not a signal in {}", name, base),
                    crate::diagnostics::Span::new(name_token.text_range().start().into()),
                );
            }
            if r.bindings
                .insert(name, Expression::Uncompiled(con_node.CodeBlock().into()))
                .is_some()
            {
                diag.push_error(
                    "Duplicated signal".into(),
                    crate::diagnostics::Span::new(name_token.text_range().start().into()),
                );
            }
        }

        for anim in node.PropertyAnimation() {
            let prop_name_token =
                anim.DeclaredIdentifier().child_token(SyntaxKind::Identifier).unwrap();

            let prop_name = prop_name_token.text().to_string();
            let prop_type = r.lookup_property(&prop_name);

            let anim_type = tr.property_animation_type_for_property(prop_type);
            if !matches!(anim_type, Type::Builtin(..)) {
                diag.push_error(
                    format!("'{}' is not an animatable property", prop_name),
                    prop_name_token.span(),
                )
            } else {
                let base =
                    QualifiedTypeName { members: vec![anim_type.as_builtin().class_name.clone()] };
                let mut anim_element = Element {
                    id: "".into(),
                    base_type: anim_type,
                    node: None,
                    ..Default::default()
                };
                anim_element.parse_bindings(&base, anim.Binding(), diag);
                let anim_element = Rc::new(RefCell::new(anim_element));
                if r.property_animations.insert(prop_name, anim_element.clone()).is_some() {
                    diag.push_error(
                        "Duplicated animation".into(),
                        crate::diagnostics::Span::new(prop_name_token.text_range().start().into()),
                    )
                }
            }
        }

        for se in node.children() {
            if se.kind() == SyntaxKind::SubElement {
                let id = se.child_text(SyntaxKind::Identifier).unwrap_or_default();
                if let Some(element_node) = se.child_node(SyntaxKind::Element) {
                    r.children.push(Rc::new(RefCell::new(Element::from_node(
                        element_node.into(),
                        id,
                        diag,
                        tr,
                    ))));
                } else {
                    assert!(diag.has_error());
                }
            } else if se.kind() == SyntaxKind::RepeatedElement {
                r.children.push(Rc::new(RefCell::new(Element::from_repeated_node(
                    se.into(),
                    diag,
                    tr,
                ))));
            }
        }
        r
    }

    fn from_repeated_node(
        node: syntax_nodes::RepeatedElement,
        diag: &mut Diagnostics,
        tr: &TypeRegister,
    ) -> Self {
        let rei = RepeatedElementInfo {
            model: Expression::Uncompiled(node.Expression().into()),
            model_data_id: node
                .DeclaredIdentifier()
                .and_then(|n| n.child_text(SyntaxKind::Identifier))
                .unwrap_or_default(),
            index_id: node
                .RepeatedIndex()
                .and_then(|r| r.child_text(SyntaxKind::Identifier))
                .unwrap_or_default(),
        };
        let mut e = Element::from_node(node.Element(), String::new(), diag, tr);
        e.repeated = Some(rei);
        e
    }

    pub fn lookup_property(&self, name: &str) -> Type {
        self.property_declarations
            .get(name)
            .cloned()
            .map(|decl| decl.property_type)
            .unwrap_or_else(|| self.base_type.lookup_property(name))
    }

    /// Return the Span of this element in the AST for error reporting
    pub fn span(&self) -> crate::diagnostics::Span {
        self.node.as_ref().map(|n| n.span()).unwrap_or_default()
    }

    fn parse_bindings(
        &mut self,
        base: &QualifiedTypeName,
        bindings: impl Iterator<Item = syntax_nodes::Binding>,
        diag: &mut Diagnostics,
    ) {
        for b in bindings {
            let name_token = match b.child_token(SyntaxKind::Identifier) {
                Some(x) => x,
                None => continue,
            };
            let name = name_token.text().to_string();
            let prop_type = self.lookup_property(&name);
            if !prop_type.is_property_type() {
                diag.push_error(
                    match prop_type {
                        Type::Invalid => format!("Unknown property {} in {}", name, base),
                        Type::Signal => format!("'{}' is a signal. Use `=>` to connect", name),
                        _ => format!("Cannot assing to {} in {}", name, base),
                    },
                    crate::diagnostics::Span::new(name_token.text_range().start().into()),
                );
            }
            if self
                .bindings
                .insert(name, Expression::Uncompiled(b.BindingExpression().into()))
                .is_some()
            {
                diag.push_error(
                    "Duplicated property binding".into(),
                    crate::diagnostics::Span::new(name_token.text_range().start().into()),
                );
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct QualifiedTypeName {
    members: Vec<String>,
}

impl QualifiedTypeName {
    pub fn from_node(node: syntax_nodes::QualifiedName) -> Self {
        debug_assert_eq!(node.kind(), SyntaxKind::QualifiedName);
        let members = node
            .children_with_tokens()
            .filter(|n| n.kind() == SyntaxKind::Identifier)
            .filter_map(|x| x.as_token().map(|x| x.text().to_string()))
            .collect();
        Self { members }
    }
}

impl std::fmt::Display for QualifiedTypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.members.join("."))
    }
}

/// Call the visitor for each children of the element recursively, starting with the element itself
///
/// The state returned by the visitor is passed to the children
pub fn recurse_elem<State>(
    elem: &ElementRc,
    state: &State,
    vis: &mut impl FnMut(&ElementRc, &State) -> State,
) {
    let state = vis(elem, state);
    for sub in &elem.borrow().children {
        recurse_elem(sub, &state, vis);
    }
}
