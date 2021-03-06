// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Reduced graph building
//!
//! Here we build the "reduced graph": the graph of the module tree without
//! any imports resolved.

use DefModifiers;
use resolve_imports::ImportDirective;
use resolve_imports::ImportDirectiveSubclass::{self, SingleImport, GlobImport};
use resolve_imports::ImportResolution;
use Module;
use ModuleKind::*;
use Namespace::{TypeNS, ValueNS};
use NameBindings;
use {names_to_string, module_to_string};
use ParentLink::{self, ModuleParentLink, BlockParentLink};
use Resolver;
use resolve_imports::Shadowable;
use TypeNsDef;
use {resolve_error, ResolutionError};

use self::DuplicateCheckingMode::*;
use self::NamespaceError::*;

use rustc::metadata::csearch;
use rustc::metadata::decoder::{DefLike, DlDef, DlField, DlImpl};
use rustc::middle::def::*;
use rustc::middle::def_id::{CRATE_DEF_INDEX, DefId};

use syntax::ast::{Name, NodeId};
use syntax::attr::AttrMetaMethods;
use syntax::parse::token::special_idents;
use syntax::codemap::{Span, DUMMY_SP};

use rustc_front::hir;
use rustc_front::hir::{Block, Crate, DeclItem};
use rustc_front::hir::{ForeignItem, ForeignItemFn, ForeignItemStatic};
use rustc_front::hir::{Item, ItemConst, ItemEnum, ItemExternCrate, ItemFn};
use rustc_front::hir::{ItemForeignMod, ItemImpl, ItemMod, ItemStatic, ItemDefaultImpl};
use rustc_front::hir::{ItemStruct, ItemTrait, ItemTy, ItemUse};
use rustc_front::hir::{NamedField, PathListIdent, PathListMod, Public};
use rustc_front::hir::StmtDecl;
use rustc_front::hir::UnnamedField;
use rustc_front::hir::{Variant, ViewPathGlob, ViewPathList, ViewPathSimple};
use rustc_front::hir::Visibility;
use rustc_front::intravisit::{self, Visitor};

use std::mem::replace;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

// Specifies how duplicates should be handled when adding a child item if
// another item exists with the same name in some namespace.
#[derive(Copy, Clone, PartialEq)]
enum DuplicateCheckingMode {
    ForbidDuplicateModules,
    ForbidDuplicateTypesAndModules,
    ForbidDuplicateValues,
    ForbidDuplicateTypesAndValues,
    OverwriteDuplicates,
}

#[derive(Copy, Clone, PartialEq)]
enum NamespaceError {
    NoError,
    ModuleError,
    TypeError,
    ValueError,
}

fn namespace_error_to_string(ns: NamespaceError) -> &'static str {
    match ns {
        NoError => "",
        ModuleError | TypeError => "type or module",
        ValueError => "value",
    }
}

struct GraphBuilder<'a, 'b: 'a, 'tcx: 'b> {
    resolver: &'a mut Resolver<'b, 'tcx>,
}

impl<'a, 'b:'a, 'tcx:'b> Deref for GraphBuilder<'a, 'b, 'tcx> {
    type Target = Resolver<'b, 'tcx>;

    fn deref(&self) -> &Resolver<'b, 'tcx> {
        &*self.resolver
    }
}

impl<'a, 'b:'a, 'tcx:'b> DerefMut for GraphBuilder<'a, 'b, 'tcx> {
    fn deref_mut(&mut self) -> &mut Resolver<'b, 'tcx> {
        &mut *self.resolver
    }
}

impl<'a, 'b:'a, 'tcx:'b> GraphBuilder<'a, 'b, 'tcx> {
    /// Constructs the reduced graph for the entire crate.
    fn build_reduced_graph(self, krate: &hir::Crate) {
        let parent = self.graph_root.get_module();
        let mut visitor = BuildReducedGraphVisitor {
            builder: self,
            parent: parent,
        };
        intravisit::walk_crate(&mut visitor, krate);
    }

    /// Adds a new child item to the module definition of the parent node and
    /// returns its corresponding name bindings as well as the current parent.
    /// Or, if we're inside a block, creates (or reuses) an anonymous module
    /// corresponding to the innermost block ID and returns the name bindings
    /// as well as the newly-created parent.
    ///
    /// # Panics
    ///
    /// Panics if this node does not have a module definition and we are not inside
    /// a block.
    fn add_child(&self,
                 name: Name,
                 parent: &Rc<Module>,
                 duplicate_checking_mode: DuplicateCheckingMode,
                 // For printing errors
                 sp: Span)
                 -> Rc<NameBindings> {
        // If this is the immediate descendant of a module, then we add the
        // child name directly. Otherwise, we create or reuse an anonymous
        // module and add the child to that.

        self.check_for_conflicts_between_external_crates_and_items(&**parent, name, sp);

        // Add or reuse the child.
        let child = parent.children.borrow().get(&name).cloned();
        match child {
            None => {
                let child = Rc::new(NameBindings::new());
                parent.children.borrow_mut().insert(name, child.clone());
                child
            }
            Some(child) => {
                // Enforce the duplicate checking mode:
                //
                // * If we're requesting duplicate module checking, check that
                //   there isn't a module in the module with the same name.
                //
                // * If we're requesting duplicate type checking, check that
                //   there isn't a type in the module with the same name.
                //
                // * If we're requesting duplicate value checking, check that
                //   there isn't a value in the module with the same name.
                //
                // * If we're requesting duplicate type checking and duplicate
                //   value checking, check that there isn't a duplicate type
                //   and a duplicate value with the same name.
                //
                // * If no duplicate checking was requested at all, do
                //   nothing.

                let mut duplicate_type = NoError;
                let ns = match duplicate_checking_mode {
                    ForbidDuplicateModules => {
                        if child.get_module_if_available().is_some() {
                            duplicate_type = ModuleError;
                        }
                        Some(TypeNS)
                    }
                    ForbidDuplicateTypesAndModules => {
                        if child.defined_in_namespace(TypeNS) {
                            duplicate_type = TypeError;
                        }
                        Some(TypeNS)
                    }
                    ForbidDuplicateValues => {
                        if child.defined_in_namespace(ValueNS) {
                            duplicate_type = ValueError;
                        }
                        Some(ValueNS)
                    }
                    ForbidDuplicateTypesAndValues => {
                        let mut n = None;
                        match child.def_for_namespace(TypeNS) {
                            Some(DefMod(_)) | None => {}
                            Some(_) => {
                                n = Some(TypeNS);
                                duplicate_type = TypeError;
                            }
                        }
                        if child.defined_in_namespace(ValueNS) {
                            duplicate_type = ValueError;
                            n = Some(ValueNS);
                        }
                        n
                    }
                    OverwriteDuplicates => None,
                };
                if duplicate_type != NoError {
                    // Return an error here by looking up the namespace that
                    // had the duplicate.
                    let ns = ns.unwrap();
                    resolve_error(
                        self,
                        sp,
                        ResolutionError::DuplicateDefinition(
                            namespace_error_to_string(duplicate_type),
                            name)
                    );
                    {
                        let r = child.span_for_namespace(ns);
                        if let Some(sp) = r {
                            self.session.span_note(sp,
                                                   &format!("first definition of {} `{}` here",
                                      namespace_error_to_string(duplicate_type),
                                      name));
                        }
                    }
                }
                child
            }
        }
    }

    fn block_needs_anonymous_module(&mut self, block: &Block) -> bool {
        // Check each statement.
        for statement in &block.stmts {
            match statement.node {
                StmtDecl(ref declaration, _) => {
                    match declaration.node {
                        DeclItem(_) => {
                            return true;
                        }
                        _ => {
                            // Keep searching.
                        }
                    }
                }
                _ => {
                    // Keep searching.
                }
            }
        }

        // If we found no items, we don't need to create
        // an anonymous module.

        return false;
    }

    fn get_parent_link(&mut self, parent: &Rc<Module>, name: Name) -> ParentLink {
        ModuleParentLink(Rc::downgrade(parent), name)
    }

    /// Constructs the reduced graph for one item.
    fn build_reduced_graph_for_item(&mut self, item: &Item, parent: &Rc<Module>) -> Rc<Module> {
        let name = item.name;
        let sp = item.span;
        let is_public = item.vis == hir::Public;
        let modifiers = if is_public {
            DefModifiers::PUBLIC
        } else {
            DefModifiers::empty()
        } | DefModifiers::IMPORTABLE;

        match item.node {
            ItemUse(ref view_path) => {
                // Extract and intern the module part of the path. For
                // globs and lists, the path is found directly in the AST;
                // for simple paths we have to munge the path a little.
                let module_path = match view_path.node {
                    ViewPathSimple(_, ref full_path) => {
                        full_path.segments
                                 .split_last()
                                 .unwrap()
                                 .1
                                 .iter()
                                 .map(|seg| seg.identifier.name)
                                 .collect()
                    }

                    ViewPathGlob(ref module_ident_path) |
                    ViewPathList(ref module_ident_path, _) => {
                        module_ident_path.segments
                                         .iter()
                                         .map(|seg| seg.identifier.name)
                                         .collect()
                    }
                };

                // Build up the import directives.
                let shadowable = item.attrs.iter().any(|attr| {
                    attr.name() == special_idents::prelude_import.name.as_str()
                });
                let shadowable = if shadowable {
                    Shadowable::Always
                } else {
                    Shadowable::Never
                };

                match view_path.node {
                    ViewPathSimple(binding, ref full_path) => {
                        let source_name = full_path.segments.last().unwrap().identifier.name;
                        if source_name.as_str() == "mod" || source_name.as_str() == "self" {
                            resolve_error(self,
                                          view_path.span,
                                          ResolutionError::SelfImportsOnlyAllowedWithin);
                        }

                        let subclass = SingleImport(binding, source_name);
                        self.build_import_directive(&**parent,
                                                    module_path,
                                                    subclass,
                                                    view_path.span,
                                                    item.id,
                                                    is_public,
                                                    shadowable);
                    }
                    ViewPathList(_, ref source_items) => {
                        // Make sure there's at most one `mod` import in the list.
                        let mod_spans = source_items.iter()
                                                    .filter_map(|item| {
                                                        match item.node {
                                                            PathListMod { .. } => Some(item.span),
                                                            _ => None,
                                                        }
                                                    })
                                                    .collect::<Vec<Span>>();
                        if mod_spans.len() > 1 {
                            resolve_error(self,
                                          mod_spans[0],
                                          ResolutionError::SelfImportCanOnlyAppearOnceInTheList);
                            for other_span in mod_spans.iter().skip(1) {
                                self.session
                                    .span_note(*other_span, "another `self` import appears here");
                            }
                        }

                        for source_item in source_items {
                            let (module_path, name, rename) = match source_item.node {
                                PathListIdent { name, rename, .. } =>
                                    (module_path.clone(), name, rename.unwrap_or(name)),
                                PathListMod { rename, .. } => {
                                    let name = match module_path.last() {
                                        Some(name) => *name,
                                        None => {
                                            resolve_error(
                                                self,
                                                source_item.span,
                                                ResolutionError::
                                                SelfImportOnlyInImportListWithNonEmptyPrefix
                                            );
                                            continue;
                                        }
                                    };
                                    let module_path = module_path.split_last().unwrap().1;
                                    let rename = rename.unwrap_or(name);
                                    (module_path.to_vec(), name, rename)
                                }
                            };
                            self.build_import_directive(&**parent,
                                                        module_path,
                                                        SingleImport(rename, name),
                                                        source_item.span,
                                                        source_item.node.id(),
                                                        is_public,
                                                        shadowable);
                        }
                    }
                    ViewPathGlob(_) => {
                        self.build_import_directive(&**parent,
                                                    module_path,
                                                    GlobImport,
                                                    view_path.span,
                                                    item.id,
                                                    is_public,
                                                    shadowable);
                    }
                }
                parent.clone()
            }

            ItemExternCrate(_) => {
                // n.b. we don't need to look at the path option here, because cstore already
                // did
                if let Some(crate_id) = self.session.cstore.find_extern_mod_stmt_cnum(item.id) {
                    let def_id = DefId {
                        krate: crate_id,
                        index: CRATE_DEF_INDEX,
                    };
                    self.external_exports.insert(def_id);
                    let parent_link = ModuleParentLink(Rc::downgrade(parent), name);
                    let external_module = Rc::new(Module::new(parent_link,
                                                              Some(def_id),
                                                              NormalModuleKind,
                                                              false,
                                                              true));
                    debug!("(build reduced graph for item) found extern `{}`",
                           module_to_string(&*external_module));
                    self.check_for_conflicts_between_external_crates(&**parent, name, sp);
                    parent.external_module_children
                          .borrow_mut()
                          .insert(name, external_module.clone());
                    self.build_reduced_graph_for_external_crate(&external_module);
                }
                parent.clone()
            }

            ItemMod(..) => {
                let child = parent.children.borrow().get(&name).cloned();
                if let Some(child) = child {
                    // check if there's struct of the same name already defined
                    if child.defined_in_namespace(TypeNS) &&
                       child.get_module_if_available().is_none() {
                        self.session.span_warn(sp,
                                               &format!("duplicate definition of {} `{}`. \
                                                         Defining a module and a struct with \
                                                         the same name will be disallowed soon.",
                                                        namespace_error_to_string(TypeError),
                                                        name));
                        {
                            let r = child.span_for_namespace(TypeNS);
                            if let Some(sp) = r {
                                self.session.span_note(sp,
                                                       &format!("first definition of {} `{}` here",
                                          namespace_error_to_string(TypeError),
                                          name));
                            }
                        }
                    }
                }
                let name_bindings = self.add_child(name, parent, ForbidDuplicateModules, sp);

                let parent_link = self.get_parent_link(parent, name);
                let def_id = self.ast_map.local_def_id(item.id);
                name_bindings.define_module(parent_link,
                                            Some(def_id),
                                            NormalModuleKind,
                                            false,
                                            is_public,
                                            sp);

                name_bindings.get_module()
            }

            ItemForeignMod(..) => parent.clone(),

            // These items live in the value namespace.
            ItemStatic(_, m, _) => {
                let name_bindings = self.add_child(name, parent, ForbidDuplicateValues, sp);
                let mutbl = m == hir::MutMutable;

                name_bindings.define_value(DefStatic(self.ast_map.local_def_id(item.id), mutbl),
                                           sp,
                                           modifiers);
                parent.clone()
            }
            ItemConst(_, _) => {
                self.add_child(name, parent, ForbidDuplicateValues, sp)
                    .define_value(DefConst(self.ast_map.local_def_id(item.id)), sp, modifiers);
                parent.clone()
            }
            ItemFn(_, _, _, _, _, _) => {
                let name_bindings = self.add_child(name, parent, ForbidDuplicateValues, sp);

                let def = DefFn(self.ast_map.local_def_id(item.id), false);
                name_bindings.define_value(def, sp, modifiers);
                parent.clone()
            }

            // These items live in the type namespace.
            ItemTy(..) => {
                let name_bindings = self.add_child(name,
                                                   parent,
                                                   ForbidDuplicateTypesAndModules,
                                                   sp);

                name_bindings.define_type(DefTy(self.ast_map.local_def_id(item.id), false),
                                          sp,
                                          modifiers);

                let parent_link = self.get_parent_link(parent, name);
                name_bindings.set_module_kind(parent_link,
                                              Some(self.ast_map.local_def_id(item.id)),
                                              TypeModuleKind,
                                              false,
                                              is_public,
                                              sp);
                parent.clone()
            }

            ItemEnum(ref enum_definition, _) => {
                let name_bindings = self.add_child(name,
                                                   parent,
                                                   ForbidDuplicateTypesAndModules,
                                                   sp);

                name_bindings.define_type(DefTy(self.ast_map.local_def_id(item.id), true),
                                          sp,
                                          modifiers);

                let parent_link = self.get_parent_link(parent, name);
                name_bindings.set_module_kind(parent_link,
                                              Some(self.ast_map.local_def_id(item.id)),
                                              EnumModuleKind,
                                              false,
                                              is_public,
                                              sp);

                let module = name_bindings.get_module();

                for variant in &(*enum_definition).variants {
                    let item_def_id = self.ast_map.local_def_id(item.id);
                    self.build_reduced_graph_for_variant(&**variant, item_def_id, &module);
                }
                parent.clone()
            }

            // These items live in both the type and value namespaces.
            ItemStruct(ref struct_def, _) => {
                // Adding to both Type and Value namespaces or just Type?
                let (forbid, ctor_id) = if struct_def.is_struct() {
                    (ForbidDuplicateTypesAndModules, None)
                } else {
                    let child = parent.children.borrow().get(&name).cloned();
                    if let Some(child) = child {
                        // check if theres a DefMod
                        if let Some(DefMod(_)) = child.def_for_namespace(TypeNS) {
                            self.session.span_warn(sp,
                                                   &format!("duplicate definition of {} `{}`. \
                                                             Defining a module and a struct \
                                                             with the same name will be \
                                                             disallowed soon.",
                                                            namespace_error_to_string(TypeError),
                                                            name));
                            {
                                let r = child.span_for_namespace(TypeNS);
                                if let Some(sp) = r {
                                    self.session
                                        .span_note(sp,
                                                   &format!("first definition of {} `{}` here",
                                                            namespace_error_to_string(TypeError),
                                                            name));
                                }
                            }
                        }
                    }
                    (ForbidDuplicateTypesAndValues, Some(struct_def.id()))
                };

                let name_bindings = self.add_child(name, parent, forbid, sp);

                // Define a name in the type namespace.
                name_bindings.define_type(DefTy(self.ast_map.local_def_id(item.id), false),
                                          sp,
                                          modifiers);

                // If this is a newtype or unit-like struct, define a name
                // in the value namespace as well
                if let Some(cid) = ctor_id {
                    name_bindings.define_value(DefStruct(self.ast_map.local_def_id(cid)),
                                               sp,
                                               modifiers);
                }

                // Record the def ID and fields of this struct.
                let named_fields = struct_def.fields()
                                             .iter()
                                             .filter_map(|f| {
                                                 match f.node.kind {
                                                     NamedField(name, _) => Some(name),
                                                     UnnamedField(_) => None,
                                                 }
                                             })
                                             .collect();
                let item_def_id = self.ast_map.local_def_id(item.id);
                self.structs.insert(item_def_id, named_fields);

                parent.clone()
            }

            ItemDefaultImpl(_, _) |
            ItemImpl(..) => parent.clone(),

            ItemTrait(_, _, _, ref items) => {
                let name_bindings = self.add_child(name,
                                                   parent,
                                                   ForbidDuplicateTypesAndModules,
                                                   sp);

                // Add all the items within to a new module.
                let parent_link = self.get_parent_link(parent, name);
                name_bindings.define_module(parent_link,
                                            Some(self.ast_map.local_def_id(item.id)),
                                            TraitModuleKind,
                                            false,
                                            is_public,
                                            sp);
                let module_parent = name_bindings.get_module();

                let def_id = self.ast_map.local_def_id(item.id);

                // Add the names of all the items to the trait info.
                for trait_item in items {
                    let name_bindings = self.add_child(trait_item.name,
                                                       &module_parent,
                                                       ForbidDuplicateTypesAndValues,
                                                       trait_item.span);

                    match trait_item.node {
                        hir::ConstTraitItem(..) => {
                            let def = DefAssociatedConst(self.ast_map.local_def_id(trait_item.id));
                            // NB: not DefModifiers::IMPORTABLE
                            name_bindings.define_value(def, trait_item.span, DefModifiers::PUBLIC);
                        }
                        hir::MethodTraitItem(..) => {
                            let def = DefMethod(self.ast_map.local_def_id(trait_item.id));
                            // NB: not DefModifiers::IMPORTABLE
                            name_bindings.define_value(def, trait_item.span, DefModifiers::PUBLIC);
                        }
                        hir::TypeTraitItem(..) => {
                            let def = DefAssociatedTy(self.ast_map.local_def_id(item.id),
                                                      self.ast_map.local_def_id(trait_item.id));
                            // NB: not DefModifiers::IMPORTABLE
                            name_bindings.define_type(def, trait_item.span, DefModifiers::PUBLIC);
                        }
                    }

                    let trait_item_def_id = self.ast_map.local_def_id(trait_item.id);
                    self.trait_item_map.insert((trait_item.name, def_id), trait_item_def_id);
                }

                name_bindings.define_type(DefTrait(def_id), sp, modifiers);
                parent.clone()
            }
        }
    }

    // Constructs the reduced graph for one variant. Variants exist in the
    // type and value namespaces.
    fn build_reduced_graph_for_variant(&mut self,
                                       variant: &Variant,
                                       item_id: DefId,
                                       parent: &Rc<Module>) {
        let name = variant.node.name;
        let is_exported = if variant.node.data.is_struct() {
            // Not adding fields for variants as they are not accessed with a self receiver
            let variant_def_id = self.ast_map.local_def_id(variant.node.data.id());
            self.structs.insert(variant_def_id, Vec::new());
            true
        } else {
            false
        };

        let child = self.add_child(name, parent, ForbidDuplicateTypesAndValues, variant.span);
        // variants are always treated as importable to allow them to be glob
        // used
        child.define_value(DefVariant(item_id,
                                      self.ast_map.local_def_id(variant.node.data.id()),
                                      is_exported),
                           variant.span,
                           DefModifiers::PUBLIC | DefModifiers::IMPORTABLE);
        child.define_type(DefVariant(item_id,
                                     self.ast_map.local_def_id(variant.node.data.id()),
                                     is_exported),
                          variant.span,
                          DefModifiers::PUBLIC | DefModifiers::IMPORTABLE);
    }

    /// Constructs the reduced graph for one foreign item.
    fn build_reduced_graph_for_foreign_item(&mut self,
                                            foreign_item: &ForeignItem,
                                            parent: &Rc<Module>) {
        let name = foreign_item.name;
        let is_public = foreign_item.vis == hir::Public;
        let modifiers = if is_public {
            DefModifiers::PUBLIC
        } else {
            DefModifiers::empty()
        } | DefModifiers::IMPORTABLE;
        let name_bindings = self.add_child(name, parent, ForbidDuplicateValues, foreign_item.span);

        let def = match foreign_item.node {
            ForeignItemFn(..) => {
                DefFn(self.ast_map.local_def_id(foreign_item.id), false)
            }
            ForeignItemStatic(_, m) => {
                DefStatic(self.ast_map.local_def_id(foreign_item.id), m)
            }
        };
        name_bindings.define_value(def, foreign_item.span, modifiers);
    }

    fn build_reduced_graph_for_block(&mut self, block: &Block, parent: &Rc<Module>) -> Rc<Module> {
        if self.block_needs_anonymous_module(block) {
            let block_id = block.id;

            debug!("(building reduced graph for block) creating a new anonymous module for block \
                    {}",
                   block_id);

            let new_module = Rc::new(Module::new(BlockParentLink(Rc::downgrade(parent), block_id),
                                                 None,
                                                 AnonymousModuleKind,
                                                 false,
                                                 false));
            parent.anonymous_children.borrow_mut().insert(block_id, new_module.clone());
            new_module
        } else {
            parent.clone()
        }
    }

    fn handle_external_def(&mut self,
                           def: Def,
                           vis: Visibility,
                           child_name_bindings: &NameBindings,
                           final_ident: &str,
                           name: Name,
                           new_parent: &Rc<Module>) {
        debug!("(building reduced graph for external crate) building external def {}, priv {:?}",
               final_ident,
               vis);
        let is_public = vis == hir::Public;
        let modifiers = if is_public {
            DefModifiers::PUBLIC
        } else {
            DefModifiers::empty()
        } | DefModifiers::IMPORTABLE;
        let is_exported = is_public &&
                          match new_parent.def_id.get() {
            None => true,
            Some(did) => self.external_exports.contains(&did),
        };
        if is_exported {
            self.external_exports.insert(def.def_id());
        }

        let kind = match def {
            DefTy(_, true) => EnumModuleKind,
            DefTy(_, false) | DefStruct(..) => TypeModuleKind,
            _ => NormalModuleKind,
        };

        match def {
            DefMod(def_id) |
            DefForeignMod(def_id) |
            DefStruct(def_id) |
            DefTy(def_id, _) => {
                let type_def = child_name_bindings.type_def.borrow().clone();
                match type_def {
                    Some(TypeNsDef { module_def: Some(module_def), .. }) => {
                        debug!("(building reduced graph for external crate) already created \
                                module");
                        module_def.def_id.set(Some(def_id));
                    }
                    Some(_) | None => {
                        debug!("(building reduced graph for external crate) building module {} {}",
                               final_ident,
                               is_public);
                        let parent_link = self.get_parent_link(new_parent, name);

                        child_name_bindings.define_module(parent_link,
                                                          Some(def_id),
                                                          kind,
                                                          true,
                                                          is_public,
                                                          DUMMY_SP);
                    }
                }
            }
            _ => {}
        }

        match def {
            DefMod(_) | DefForeignMod(_) => {}
            DefVariant(_, variant_id, is_struct) => {
                debug!("(building reduced graph for external crate) building variant {}",
                       final_ident);
                // variants are always treated as importable to allow them to be
                // glob used
                let modifiers = DefModifiers::PUBLIC | DefModifiers::IMPORTABLE;
                if is_struct {
                    child_name_bindings.define_type(def, DUMMY_SP, modifiers);
                    // Not adding fields for variants as they are not accessed with a self receiver
                    self.structs.insert(variant_id, Vec::new());
                } else {
                    child_name_bindings.define_value(def, DUMMY_SP, modifiers);
                }
            }
            DefFn(ctor_id, true) => {
                child_name_bindings.define_value(
                csearch::get_tuple_struct_definition_if_ctor(&self.session.cstore, ctor_id)
                    .map_or(def, |_| DefStruct(ctor_id)), DUMMY_SP, modifiers);
            }
            DefFn(..) |
            DefStatic(..) |
            DefConst(..) |
            DefAssociatedConst(..) |
            DefMethod(..) => {
                debug!("(building reduced graph for external crate) building value (fn/static) {}",
                       final_ident);
                // impl methods have already been defined with the correct importability
                // modifier
                let mut modifiers = match *child_name_bindings.value_def.borrow() {
                    Some(ref def) => (modifiers & !DefModifiers::IMPORTABLE) |
                                     (def.modifiers & DefModifiers::IMPORTABLE),
                    None => modifiers,
                };
                if new_parent.kind.get() != NormalModuleKind {
                    modifiers = modifiers & !DefModifiers::IMPORTABLE;
                }
                child_name_bindings.define_value(def, DUMMY_SP, modifiers);
            }
            DefTrait(def_id) => {
                debug!("(building reduced graph for external crate) building type {}",
                       final_ident);

                // If this is a trait, add all the trait item names to the trait
                // info.

                let trait_item_def_ids = csearch::get_trait_item_def_ids(&self.session.cstore,
                                                                         def_id);
                for trait_item_def in &trait_item_def_ids {
                    let trait_item_name = csearch::get_trait_name(&self.session.cstore,
                                                                  trait_item_def.def_id());

                    debug!("(building reduced graph for external crate) ... adding trait item \
                            '{}'",
                           trait_item_name);

                    self.trait_item_map.insert((trait_item_name, def_id), trait_item_def.def_id());

                    if is_exported {
                        self.external_exports.insert(trait_item_def.def_id());
                    }
                }

                child_name_bindings.define_type(def, DUMMY_SP, modifiers);

                // Define a module if necessary.
                let parent_link = self.get_parent_link(new_parent, name);
                child_name_bindings.set_module_kind(parent_link,
                                                    Some(def_id),
                                                    TraitModuleKind,
                                                    true,
                                                    is_public,
                                                    DUMMY_SP)
            }
            DefTy(..) | DefAssociatedTy(..) => {
                debug!("(building reduced graph for external crate) building type {}",
                       final_ident);

                let modifiers = match new_parent.kind.get() {
                    NormalModuleKind => modifiers,
                    _ => modifiers & !DefModifiers::IMPORTABLE,
                };

                child_name_bindings.define_type(def, DUMMY_SP, modifiers);
            }
            DefStruct(def_id) => {
                debug!("(building reduced graph for external crate) building type and value for \
                        {}",
                       final_ident);
                child_name_bindings.define_type(def, DUMMY_SP, modifiers);
                let fields = csearch::get_struct_field_names(&self.session.cstore, def_id);

                if fields.is_empty() {
                    child_name_bindings.define_value(def, DUMMY_SP, modifiers);
                }

                // Record the def ID and fields of this struct.
                self.structs.insert(def_id, fields);
            }
            DefLocal(..) |
            DefPrimTy(..) |
            DefTyParam(..) |
            DefUse(..) |
            DefUpvar(..) |
            DefLabel(..) |
            DefSelfTy(..) => {
                panic!("didn't expect `{:?}`", def);
            }
        }
    }

    /// Builds the reduced graph for a single item in an external crate.
    fn build_reduced_graph_for_external_crate_def(&mut self,
                                                  root: &Rc<Module>,
                                                  def_like: DefLike,
                                                  name: Name,
                                                  def_visibility: Visibility) {
        match def_like {
            DlDef(def) => {
                // Add the new child item, if necessary.
                match def {
                    DefForeignMod(def_id) => {
                        // Foreign modules have no names. Recur and populate
                        // eagerly.
                        csearch::each_child_of_item(&self.session.cstore,
                                                    def_id,
                                                    |def_like,
                                                     child_name,
                                                     vis| {
                            self.build_reduced_graph_for_external_crate_def(
                                root,
                                def_like,
                                child_name,
                                vis)
                        });
                    }
                    _ => {
                        let child_name_bindings = self.add_child(name,
                                                                 root,
                                                                 OverwriteDuplicates,
                                                                 DUMMY_SP);

                        self.handle_external_def(def,
                                                 def_visibility,
                                                 &*child_name_bindings,
                                                 &name.as_str(),
                                                 name,
                                                 root);
                    }
                }
            }
            DlImpl(_) => {
                debug!("(building reduced graph for external crate) ignoring impl");
            }
            DlField => {
                debug!("(building reduced graph for external crate) ignoring field");
            }
        }
    }

    /// Builds the reduced graph rooted at the given external module.
    fn populate_external_module(&mut self, module: &Rc<Module>) {
        debug!("(populating external module) attempting to populate {}",
               module_to_string(&**module));

        let def_id = match module.def_id.get() {
            None => {
                debug!("(populating external module) ... no def ID!");
                return;
            }
            Some(def_id) => def_id,
        };

        csearch::each_child_of_item(&self.session.cstore,
                                    def_id,
                                    |def_like, child_name, visibility| {
                                        debug!("(populating external module) ... found ident: {}",
                                               child_name);
                                        self.build_reduced_graph_for_external_crate_def(module,
                                                                                        def_like,
                                                                                        child_name,
                                                                                        visibility)
                                    });
        module.populated.set(true)
    }

    /// Ensures that the reduced graph rooted at the given external module
    /// is built, building it if it is not.
    fn populate_module_if_necessary(&mut self, module: &Rc<Module>) {
        if !module.populated.get() {
            self.populate_external_module(module)
        }
        assert!(module.populated.get())
    }

    /// Builds the reduced graph rooted at the 'use' directive for an external
    /// crate.
    fn build_reduced_graph_for_external_crate(&mut self, root: &Rc<Module>) {
        csearch::each_top_level_item_of_crate(&self.session.cstore,
                                              root.def_id
                                                  .get()
                                                  .unwrap()
                                                  .krate,
                                              |def_like, name, visibility| {
            self.build_reduced_graph_for_external_crate_def(root, def_like, name, visibility)
        });
    }

    /// Creates and adds an import directive to the given module.
    fn build_import_directive(&mut self,
                              module_: &Module,
                              module_path: Vec<Name>,
                              subclass: ImportDirectiveSubclass,
                              span: Span,
                              id: NodeId,
                              is_public: bool,
                              shadowable: Shadowable) {
        module_.imports
               .borrow_mut()
               .push(ImportDirective::new(module_path, subclass, span, id, is_public, shadowable));
        self.unresolved_imports += 1;

        if is_public {
            module_.inc_pub_count();
        }

        // Bump the reference count on the name. Or, if this is a glob, set
        // the appropriate flag.

        match subclass {
            SingleImport(target, _) => {
                debug!("(building import directive) building import directive: {}::{}",
                       names_to_string(&module_.imports.borrow().last().unwrap().module_path),
                       target);

                let mut import_resolutions = module_.import_resolutions.borrow_mut();
                match import_resolutions.get_mut(&target) {
                    Some(resolution) => {
                        debug!("(building import directive) bumping reference");
                        resolution.outstanding_references += 1;

                        // the source of this name is different now
                        resolution.type_id = id;
                        resolution.value_id = id;
                        resolution.is_public = is_public;
                        return;
                    }
                    None => {}
                }
                debug!("(building import directive) creating new");
                let mut resolution = ImportResolution::new(id, is_public);
                resolution.outstanding_references = 1;
                import_resolutions.insert(target, resolution);
            }
            GlobImport => {
                // Set the glob flag. This tells us that we don't know the
                // module's exports ahead of time.

                module_.inc_glob_count();
                if is_public {
                    module_.inc_pub_glob_count();
                }
            }
        }
    }
}

struct BuildReducedGraphVisitor<'a, 'b: 'a, 'tcx: 'b> {
    builder: GraphBuilder<'a, 'b, 'tcx>,
    parent: Rc<Module>,
}

impl<'a, 'b, 'v, 'tcx> Visitor<'v> for BuildReducedGraphVisitor<'a, 'b, 'tcx> {
    fn visit_nested_item(&mut self, item: hir::ItemId) {
        self.visit_item(self.builder.resolver.ast_map.expect_item(item.id))
    }

    fn visit_item(&mut self, item: &Item) {
        let p = self.builder.build_reduced_graph_for_item(item, &self.parent);
        let old_parent = replace(&mut self.parent, p);
        intravisit::walk_item(self, item);
        self.parent = old_parent;
    }

    fn visit_foreign_item(&mut self, foreign_item: &ForeignItem) {
        self.builder.build_reduced_graph_for_foreign_item(foreign_item, &self.parent);
    }

    fn visit_block(&mut self, block: &Block) {
        let np = self.builder.build_reduced_graph_for_block(block, &self.parent);
        let old_parent = replace(&mut self.parent, np);
        intravisit::walk_block(self, block);
        self.parent = old_parent;
    }
}

pub fn build_reduced_graph(resolver: &mut Resolver, krate: &hir::Crate) {
    GraphBuilder { resolver: resolver }.build_reduced_graph(krate);
}

pub fn populate_module_if_necessary(resolver: &mut Resolver, module: &Rc<Module>) {
    GraphBuilder { resolver: resolver }.populate_module_if_necessary(module);
}
