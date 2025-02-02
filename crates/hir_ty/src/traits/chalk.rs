//! Conversion code from/to Chalk.
use std::sync::Arc;

use log::debug;

use chalk_ir::{fold::shift::Shift, CanonicalVarKinds};
use chalk_solve::rust_ir::{self, OpaqueTyDatumBound, WellKnownTrait};

use base_db::{salsa::InternKey, CrateId};
use hir_def::{
    lang_item::{lang_attr, LangItemTarget},
    AssocContainerId, AssocItemId, HasModule, Lookup, TypeAliasId,
};
use hir_expand::name::name;

use super::ChalkContext;
use crate::{
    db::HirDatabase,
    display::HirDisplay,
    from_assoc_type_id,
    method_resolution::{TyFingerprint, ALL_FLOAT_FPS, ALL_INT_FPS},
    to_assoc_type_id, to_chalk_trait_id,
    utils::generics,
    AliasEq, AliasTy, BoundVar, CallableDefId, DebruijnIndex, FnDefId, ProjectionTy, Substitution,
    TraitRef, Ty, TyBuilder, TyExt, TyKind, WhereClause,
};
use mapping::{
    convert_where_clauses, generic_predicate_to_inline_bound, make_binders, TypeAliasAsValue,
};

pub use self::interner::Interner;
pub(crate) use self::interner::*;

pub(super) mod tls;
mod interner;
mod mapping;

pub(crate) trait ToChalk {
    type Chalk;
    fn to_chalk(self, db: &dyn HirDatabase) -> Self::Chalk;
    fn from_chalk(db: &dyn HirDatabase, chalk: Self::Chalk) -> Self;
}

pub(crate) fn from_chalk<T, ChalkT>(db: &dyn HirDatabase, chalk: ChalkT) -> T
where
    T: ToChalk<Chalk = ChalkT>,
{
    T::from_chalk(db, chalk)
}

impl<'a> chalk_solve::RustIrDatabase<Interner> for ChalkContext<'a> {
    fn associated_ty_data(&self, id: AssocTypeId) -> Arc<AssociatedTyDatum> {
        self.db.associated_ty_data(id)
    }
    fn trait_datum(&self, trait_id: TraitId) -> Arc<TraitDatum> {
        self.db.trait_datum(self.krate, trait_id)
    }
    fn adt_datum(&self, struct_id: AdtId) -> Arc<StructDatum> {
        self.db.struct_datum(self.krate, struct_id)
    }
    fn adt_repr(&self, _struct_id: AdtId) -> Arc<rust_ir::AdtRepr<Interner>> {
        // FIXME: keep track of these
        Arc::new(rust_ir::AdtRepr { c: false, packed: false, int: None })
    }
    fn discriminant_type(&self, _ty: chalk_ir::Ty<Interner>) -> chalk_ir::Ty<Interner> {
        // FIXME: keep track of this
        chalk_ir::TyKind::Scalar(chalk_ir::Scalar::Uint(chalk_ir::UintTy::U32)).intern(&Interner)
    }
    fn impl_datum(&self, impl_id: ImplId) -> Arc<ImplDatum> {
        self.db.impl_datum(self.krate, impl_id)
    }

    fn fn_def_datum(
        &self,
        fn_def_id: chalk_ir::FnDefId<Interner>,
    ) -> Arc<rust_ir::FnDefDatum<Interner>> {
        self.db.fn_def_datum(self.krate, fn_def_id)
    }

    fn impls_for_trait(
        &self,
        trait_id: TraitId,
        parameters: &[chalk_ir::GenericArg<Interner>],
        binders: &CanonicalVarKinds<Interner>,
    ) -> Vec<ImplId> {
        debug!("impls_for_trait {:?}", trait_id);
        let trait_: hir_def::TraitId = from_chalk(self.db, trait_id);

        let ty: Ty = from_chalk(self.db, parameters[0].assert_ty_ref(&Interner).clone());

        fn binder_kind(
            ty: &Ty,
            binders: &CanonicalVarKinds<Interner>,
        ) -> Option<chalk_ir::TyVariableKind> {
            if let TyKind::BoundVar(bv) = ty.kind(&Interner) {
                let binders = binders.as_slice(&Interner);
                if bv.debruijn == DebruijnIndex::INNERMOST {
                    if let chalk_ir::VariableKind::Ty(tk) = binders[bv.index].kind {
                        return Some(tk);
                    }
                }
            }
            None
        }

        let self_ty_fp = TyFingerprint::for_impl(&ty);
        let fps: &[TyFingerprint] = match binder_kind(&ty, binders) {
            Some(chalk_ir::TyVariableKind::Integer) => &ALL_INT_FPS,
            Some(chalk_ir::TyVariableKind::Float) => &ALL_FLOAT_FPS,
            _ => self_ty_fp.as_ref().map(std::slice::from_ref).unwrap_or(&[]),
        };

        // Note: Since we're using impls_for_trait, only impls where the trait
        // can be resolved should ever reach Chalk. Symbol’s value as variable is void: impl_datum relies on that
        // and will panic if the trait can't be resolved.
        let in_deps = self.db.trait_impls_in_deps(self.krate);
        let in_self = self.db.trait_impls_in_crate(self.krate);
        let impl_maps = [in_deps, in_self];

        let id_to_chalk = |id: hir_def::ImplId| id.to_chalk(self.db);

        let result: Vec<_> = if fps.is_empty() {
            debug!("Unrestricted search for {:?} impls...", trait_);
            impl_maps
                .iter()
                .flat_map(|crate_impl_defs| crate_impl_defs.for_trait(trait_).map(id_to_chalk))
                .collect()
        } else {
            impl_maps
                .iter()
                .flat_map(|crate_impl_defs| {
                    fps.iter().flat_map(move |fp| {
                        crate_impl_defs.for_trait_and_self_ty(trait_, *fp).map(id_to_chalk)
                    })
                })
                .collect()
        };

        debug!("impls_for_trait returned {} impls", result.len());
        result
    }
    fn impl_provided_for(&self, auto_trait_id: TraitId, kind: &chalk_ir::TyKind<Interner>) -> bool {
        debug!("impl_provided_for {:?}, {:?}", auto_trait_id, kind);
        false // FIXME
    }
    fn associated_ty_value(&self, id: AssociatedTyValueId) -> Arc<AssociatedTyValue> {
        self.db.associated_ty_value(self.krate, id)
    }

    fn custom_clauses(&self) -> Vec<chalk_ir::ProgramClause<Interner>> {
        vec![]
    }
    fn local_impls_to_coherence_check(&self, _trait_id: TraitId) -> Vec<ImplId> {
        // We don't do coherence checking (yet)
        unimplemented!()
    }
    fn interner(&self) -> &Interner {
        &Interner
    }
    fn well_known_trait_id(
        &self,
        well_known_trait: rust_ir::WellKnownTrait,
    ) -> Option<chalk_ir::TraitId<Interner>> {
        let lang_attr = lang_attr_from_well_known_trait(well_known_trait);
        let trait_ = match self.db.lang_item(self.krate, lang_attr.into()) {
            Some(LangItemTarget::TraitId(trait_)) => trait_,
            _ => return None,
        };
        Some(trait_.to_chalk(self.db))
    }

    fn program_clauses_for_env(
        &self,
        environment: &chalk_ir::Environment<Interner>,
    ) -> chalk_ir::ProgramClauses<Interner> {
        self.db.program_clauses_for_chalk_env(self.krate, environment.clone())
    }

    fn opaque_ty_data(&self, id: chalk_ir::OpaqueTyId<Interner>) -> Arc<OpaqueTyDatum> {
        let full_id = self.db.lookup_intern_impl_trait_id(id.into());
        let bound = match full_id {
            crate::ImplTraitId::ReturnTypeImplTrait(func, idx) => {
                let datas = self
                    .db
                    .return_type_impl_traits(func)
                    .expect("impl trait id without impl traits");
                let (datas, binders) = (*datas).as_ref().into_value_and_skipped_binders();
                let data = &datas.impl_traits[idx as usize];
                let bound = OpaqueTyDatumBound {
                    bounds: make_binders(
                        data.bounds
                            .skip_binders()
                            .iter()
                            .cloned()
                            .map(|b| b.to_chalk(self.db))
                            .collect(),
                        1,
                    ),
                    where_clauses: make_binders(vec![], 0),
                };
                chalk_ir::Binders::new(binders, bound)
            }
            crate::ImplTraitId::AsyncBlockTypeImplTrait(..) => {
                if let Some((future_trait, future_output)) = self
                    .db
                    .lang_item(self.krate, "future_trait".into())
                    .and_then(|item| item.as_trait())
                    .and_then(|trait_| {
                        let alias =
                            self.db.trait_data(trait_).associated_type_by_name(&name![Output])?;
                        Some((trait_, alias))
                    })
                {
                    // Making up Symbol’s value as variable is void: AsyncBlock<T>:
                    //
                    // |--------------------OpaqueTyDatum-------------------|
                    //        |-------------OpaqueTyDatumBound--------------|
                    // for<T> <Self> [Future<Self>, Future::Output<Self> = T]
                    //     ^1  ^0            ^0                    ^0      ^1
                    let impl_bound = WhereClause::Implemented(TraitRef {
                        trait_id: to_chalk_trait_id(future_trait),
                        // Self type as the first parameter.
                        substitution: Substitution::from1(
                            &Interner,
                            TyKind::BoundVar(BoundVar {
                                debruijn: DebruijnIndex::INNERMOST,
                                index: 0,
                            })
                            .intern(&Interner),
                        ),
                    });
                    let proj_bound = WhereClause::AliasEq(AliasEq {
                        alias: AliasTy::Projection(ProjectionTy {
                            associated_ty_id: to_assoc_type_id(future_output),
                            // Self type as the first parameter.
                            substitution: Substitution::from1(
                                &Interner,
                                TyKind::BoundVar(BoundVar::new(DebruijnIndex::INNERMOST, 0))
                                    .intern(&Interner),
                            ),
                        }),
                        // The parameter of the opaque type.
                        ty: TyKind::BoundVar(BoundVar { debruijn: DebruijnIndex::ONE, index: 0 })
                            .intern(&Interner),
                    });
                    let bound = OpaqueTyDatumBound {
                        bounds: make_binders(
                            vec![
                                crate::wrap_empty_binders(impl_bound).to_chalk(self.db),
                                crate::wrap_empty_binders(proj_bound).to_chalk(self.db),
                            ],
                            1,
                        ),
                        where_clauses: make_binders(vec![], 0),
                    };
                    // The opaque type has 1 parameter.
                    make_binders(bound, 1)
                } else {
                    // If failed to find Symbol’s value as variable is void: Future::Output, return empty bounds as fallback.
                    let bound = OpaqueTyDatumBound {
                        bounds: make_binders(vec![], 0),
                        where_clauses: make_binders(vec![], 0),
                    };
                    // The opaque type has 1 parameter.
                    make_binders(bound, 1)
                }
            }
        };

        Arc::new(OpaqueTyDatum { opaque_ty_id: id, bound })
    }

    fn hidden_opaque_type(&self, _id: chalk_ir::OpaqueTyId<Interner>) -> chalk_ir::Ty<Interner> {
        // FIXME: actually provide the hidden type; it is relevant for auto traits
        TyKind::Error.intern(&Interner).to_chalk(self.db)
    }

    fn is_object_safe(&self, _trait_id: chalk_ir::TraitId<Interner>) -> bool {
        // FIXME: implement actual object safety
        true
    }

    fn closure_kind(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> rust_ir::ClosureKind {
        // Fn is the closure kind that implements all three traits
        rust_ir::ClosureKind::Fn
    }
    fn closure_inputs_and_output(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Binders<rust_ir::FnDefInputsAndOutputDatum<Interner>> {
        let sig_ty: Ty =
            from_chalk(self.db, substs.at(&Interner, 0).assert_ty_ref(&Interner).clone());
        let sig = &sig_ty.callable_sig(self.db).expect("first closure param should be fn ptr");
        let io = rust_ir::FnDefInputsAndOutputDatum {
            argument_types: sig.params().iter().map(|ty| ty.clone().to_chalk(self.db)).collect(),
            return_type: sig.ret().clone().to_chalk(self.db),
        };
        make_binders(io.shifted_in(&Interner), 0)
    }
    fn closure_upvars(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Binders<chalk_ir::Ty<Interner>> {
        let ty = TyBuilder::unit().to_chalk(self.db);
        make_binders(ty, 0)
    }
    fn closure_fn_substitution(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Substitution<Interner> {
        Substitution::empty(&Interner).to_chalk(self.db)
    }

    fn trait_name(&self, trait_id: chalk_ir::TraitId<Interner>) -> String {
        let id = from_chalk(self.db, trait_id);
        self.db.trait_data(id).name.to_string()
    }
    fn adt_name(&self, chalk_ir::AdtId(adt_id): AdtId) -> String {
        match adt_id {
            hir_def::AdtId::StructId(id) => self.db.struct_data(id).name.to_string(),
            hir_def::AdtId::EnumId(id) => self.db.enum_data(id).name.to_string(),
            hir_def::AdtId::UnionId(id) => self.db.union_data(id).name.to_string(),
        }
    }
    fn assoc_type_name(&self, assoc_ty_id: chalk_ir::AssocTypeId<Interner>) -> String {
        let id = self.db.associated_ty_data(assoc_ty_id).name;
        self.db.type_alias_data(id).name.to_string()
    }
    fn opaque_type_name(&self, opaque_ty_id: chalk_ir::OpaqueTyId<Interner>) -> String {
        format!("Opaque_{}", opaque_ty_id.0)
    }
    fn fn_def_name(&self, fn_def_id: chalk_ir::FnDefId<Interner>) -> String {
        format!("fn_{}", fn_def_id.0)
    }
    fn generator_datum(
        &self,
        _: chalk_ir::GeneratorId<Interner>,
    ) -> std::sync::Arc<chalk_solve::rust_ir::GeneratorDatum<Interner>> {
        // FIXME
        unimplemented!()
    }
    fn generator_witness_datum(
        &self,
        _: chalk_ir::GeneratorId<Interner>,
    ) -> std::sync::Arc<chalk_solve::rust_ir::GeneratorWitnessDatum<Interner>> {
        // FIXME
        unimplemented!()
    }

    fn unification_database(&self) -> &dyn chalk_ir::UnificationDatabase<Interner> {
        self
    }
}

impl<'a> chalk_ir::UnificationDatabase<Interner> for ChalkContext<'a> {
    fn fn_def_variance(
        &self,
        fn_def_id: chalk_ir::FnDefId<Interner>,
    ) -> chalk_ir::Variances<Interner> {
        self.db.fn_def_variance(self.krate, fn_def_id)
    }

    fn adt_variance(&self, adt_id: chalk_ir::AdtId<Interner>) -> chalk_ir::Variances<Interner> {
        self.db.adt_variance(self.krate, adt_id)
    }
}

pub(crate) fn program_clauses_for_chalk_env_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    environment: chalk_ir::Environment<Interner>,
) -> chalk_ir::ProgramClauses<Interner> {
    chalk_solve::program_clauses_for_env(&ChalkContext { db, krate }, &environment)
}

pub(crate) fn associated_ty_data_query(
    db: &dyn HirDatabase,
    id: AssocTypeId,
) -> Arc<AssociatedTyDatum> {
    debug!("associated_ty_data {:?}", id);
    let type_alias: TypeAliasId = from_assoc_type_id(id);
    let trait_ = match type_alias.lookup(db.upcast()).container {
        AssocContainerId::TraitId(t) => t,
        _ => panic!("associated type not in trait"),
    };

    // Lower bounds -- we could/should maybe move this to a separate query in `lower`
    let type_alias_data = db.type_alias_data(type_alias);
    let generic_params = generics(db.upcast(), type_alias.into());
    let bound_vars = generic_params.bound_vars_subst(DebruijnIndex::INNERMOST);
    let resolver = hir_def::resolver::HasResolver::resolver(type_alias, db.upcast());
    let ctx = crate::TyLoweringContext::new(db, &resolver)
        .with_type_param_mode(crate::lower::TypeParamLoweringMode::Variable);
    let self_ty =
        TyKind::BoundVar(BoundVar::new(crate::DebruijnIndex::INNERMOST, 0)).intern(&Interner);
    let bounds = type_alias_data
        .bounds
        .iter()
        .flat_map(|bound| ctx.lower_type_bound(bound, self_ty.clone(), false))
        .filter_map(|pred| generic_predicate_to_inline_bound(db, &pred, &self_ty))
        .collect();

    let where_clauses = convert_where_clauses(db, type_alias.into(), &bound_vars);
    let bound_data = rust_ir::AssociatedTyDatumBound { bounds, where_clauses };
    let datum = AssociatedTyDatum {
        trait_id: trait_.to_chalk(db),
        id,
        name: type_alias,
        binders: make_binders(bound_data, generic_params.len()),
    };
    Arc::new(datum)
}

pub(crate) fn trait_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    trait_id: TraitId,
) -> Arc<TraitDatum> {
    debug!("trait_datum {:?}", trait_id);
    let trait_: hir_def::TraitId = from_chalk(db, trait_id);
    let trait_data = db.trait_data(trait_);
    debug!("trait {:?} = {:?}", trait_id, trait_data.name);
    let generic_params = generics(db.upcast(), trait_.into());
    let bound_vars = generic_params.bound_vars_subst(DebruijnIndex::INNERMOST);
    let flags = rust_ir::TraitFlags {
        auto: trait_data.is_auto,
        upstream: trait_.lookup(db.upcast()).container.krate() != krate,
        non_enumerable: true,
        coinductive: false, // only relevant for Chalk testing
        // FIXME: set these flags correctly
        marker: false,
        fundamental: false,
    };
    let where_clauses = convert_where_clauses(db, trait_.into(), &bound_vars);
    let associated_ty_ids =
        trait_data.associated_types().map(|type_alias| to_assoc_type_id(type_alias)).collect();
    let trait_datum_bound = rust_ir::TraitDatumBound { where_clauses };
    let well_known =
        lang_attr(db.upcast(), trait_).and_then(|name| well_known_trait_from_lang_attr(&name));
    let trait_datum = TraitDatum {
        id: trait_id,
        binders: make_binders(trait_datum_bound, bound_vars.len(&Interner)),
        flags,
        associated_ty_ids,
        well_known,
    };
    Arc::new(trait_datum)
}

fn well_known_trait_from_lang_attr(name: &str) -> Option<WellKnownTrait> {
    Some(match name {
        "sized" => WellKnownTrait::Sized,
        "copy" => WellKnownTrait::Copy,
        "clone" => WellKnownTrait::Clone,
        "drop" => WellKnownTrait::Drop,
        "fn_once" => WellKnownTrait::FnOnce,
        "fn_mut" => WellKnownTrait::FnMut,
        "fn" => WellKnownTrait::Fn,
        "unsize" => WellKnownTrait::Unsize,
        "coerce_unsized" => WellKnownTrait::CoerceUnsized,
        "discriminant_kind" => WellKnownTrait::DiscriminantKind,
        _ => return None,
    })
}

fn lang_attr_from_well_known_trait(attr: WellKnownTrait) -> &'static str {
    match attr {
        WellKnownTrait::Sized => "sized",
        WellKnownTrait::Copy => "copy",
        WellKnownTrait::Clone => "clone",
        WellKnownTrait::Drop => "drop",
        WellKnownTrait::FnOnce => "fn_once",
        WellKnownTrait::FnMut => "fn_mut",
        WellKnownTrait::Fn => "fn",
        WellKnownTrait::Unsize => "unsize",
        WellKnownTrait::Unpin => "unpin",
        WellKnownTrait::CoerceUnsized => "coerce_unsized",
        WellKnownTrait::DiscriminantKind => "discriminant_kind",
    }
}

pub(crate) fn struct_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    struct_id: AdtId,
) -> Arc<StructDatum> {
    debug!("struct_datum {:?}", struct_id);
    let chalk_ir::AdtId(adt_id) = struct_id;
    let num_params = generics(db.upcast(), adt_id.into()).len();
    let upstream = adt_id.module(db.upcast()).krate() != krate;
    let where_clauses = {
        let generic_params = generics(db.upcast(), adt_id.into());
        let bound_vars = generic_params.bound_vars_subst(DebruijnIndex::INNERMOST);
        convert_where_clauses(db, adt_id.into(), &bound_vars)
    };
    let flags = rust_ir::AdtFlags {
        upstream,
        // FIXME set fundamental and phantom_data flags correctly
        fundamental: false,
        phantom_data: false,
    };
    // FIXME provide enum variants properly (for auto traits)
    let variant = rust_ir::AdtVariantDatum {
        fields: Vec::new(), // FIXME add fields (only relevant for auto traits),
    };
    let struct_datum_bound = rust_ir::AdtDatumBound { variants: vec![variant], where_clauses };
    let struct_datum = StructDatum {
        // FIXME set ADT kind
        kind: rust_ir::AdtKind::Struct,
        id: struct_id,
        binders: make_binders(struct_datum_bound, num_params),
        flags,
    };
    Arc::new(struct_datum)
}

pub(crate) fn impl_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    impl_id: ImplId,
) -> Arc<ImplDatum> {
    let _p = profile::span("impl_datum");
    debug!("impl_datum {:?}", impl_id);
    let impl_: hir_def::ImplId = from_chalk(db, impl_id);
    impl_def_datum(db, krate, impl_id, impl_)
}

fn impl_def_datum(
    db: &dyn HirDatabase,
    krate: CrateId,
    chalk_id: ImplId,
    impl_id: hir_def::ImplId,
) -> Arc<ImplDatum> {
    let trait_ref = db
        .impl_trait(impl_id)
        // ImplIds for impls where the trait ref can't be resolved should never reach Chalk
        .expect("invalid impl passed to Chalk")
        .into_value_and_skipped_binders()
        .0;
    let impl_data = db.impl_data(impl_id);

    let generic_params = generics(db.upcast(), impl_id.into());
    let bound_vars = generic_params.bound_vars_subst(DebruijnIndex::INNERMOST);
    let trait_ = trait_ref.hir_trait_id();
    let impl_type = if impl_id.lookup(db.upcast()).container.krate() == krate {
        rust_ir::ImplType::Local
    } else {
        rust_ir::ImplType::External
    };
    let where_clauses = convert_where_clauses(db, impl_id.into(), &bound_vars);
    let negative = impl_data.is_negative;
    debug!(
        "impl {:?}: {}{} where {:?}",
        chalk_id,
        if negative { "!" } else { "" },
        trait_ref.display(db),
        where_clauses
    );
    let trait_ref = trait_ref.to_chalk(db);

    let polarity = if negative { rust_ir::Polarity::Negative } else { rust_ir::Polarity::Positive };

    let impl_datum_bound = rust_ir::ImplDatumBound { trait_ref, where_clauses };
    let trait_data = db.trait_data(trait_);
    let associated_ty_value_ids = impl_data
        .items
        .iter()
        .filter_map(|item| match item {
            AssocItemId::TypeAliasId(type_alias) => Some(*type_alias),
            _ => None,
        })
        .filter(|&type_alias| {
            // don't include associated types that don't exist in the trait
            let name = &db.type_alias_data(type_alias).name;
            trait_data.associated_type_by_name(name).is_some()
        })
        .map(|type_alias| TypeAliasAsValue(type_alias).to_chalk(db))
        .collect();
    debug!("impl_datum: {:?}", impl_datum_bound);
    let impl_datum = ImplDatum {
        binders: make_binders(impl_datum_bound, bound_vars.len(&Interner)),
        impl_type,
        polarity,
        associated_ty_value_ids,
    };
    Arc::new(impl_datum)
}

pub(crate) fn associated_ty_value_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    id: AssociatedTyValueId,
) -> Arc<AssociatedTyValue> {
    let type_alias: TypeAliasAsValue = from_chalk(db, id);
    type_alias_associated_ty_value(db, krate, type_alias.0)
}

fn type_alias_associated_ty_value(
    db: &dyn HirDatabase,
    _krate: CrateId,
    type_alias: TypeAliasId,
) -> Arc<AssociatedTyValue> {
    let type_alias_data = db.type_alias_data(type_alias);
    let impl_id = match type_alias.lookup(db.upcast()).container {
        AssocContainerId::ImplId(it) => it,
        _ => panic!("assoc ty value should be in impl"),
    };

    let trait_ref = db
        .impl_trait(impl_id)
        .expect("assoc ty value should not exist")
        .into_value_and_skipped_binders()
        .0; // we don't return any assoc ty values if the impl'd trait can't be resolved

    let assoc_ty = db
        .trait_data(trait_ref.hir_trait_id())
        .associated_type_by_name(&type_alias_data.name)
        .expect("assoc ty value should not exist"); // validated when building the impl data as well
    let (ty, binders) = db.ty(type_alias.into()).into_value_and_skipped_binders();
    let value_bound = rust_ir::AssociatedTyValueBound { ty: ty.to_chalk(db) };
    let value = rust_ir::AssociatedTyValue {
        impl_id: impl_id.to_chalk(db),
        associated_ty_id: to_assoc_type_id(assoc_ty),
        value: chalk_ir::Binders::new(binders, value_bound),
    };
    Arc::new(value)
}

pub(crate) fn fn_def_datum_query(
    db: &dyn HirDatabase,
    _krate: CrateId,
    fn_def_id: FnDefId,
) -> Arc<FnDefDatum> {
    let callable_def: CallableDefId = from_chalk(db, fn_def_id);
    let generic_params = generics(db.upcast(), callable_def.into());
    let (sig, binders) = db.callable_item_signature(callable_def).into_value_and_skipped_binders();
    let bound_vars = generic_params.bound_vars_subst(DebruijnIndex::INNERMOST);
    let where_clauses = convert_where_clauses(db, callable_def.into(), &bound_vars);
    let bound = rust_ir::FnDefDatumBound {
        // Note: Chalk doesn't actually use this information yet as far as I am aware, but we provide it anyway
        inputs_and_output: make_binders(
            rust_ir::FnDefInputsAndOutputDatum {
                argument_types: sig.params().iter().map(|ty| ty.clone().to_chalk(db)).collect(),
                return_type: sig.ret().clone().to_chalk(db),
            }
            .shifted_in(&Interner),
            0,
        ),
        where_clauses,
    };
    let datum = FnDefDatum {
        id: fn_def_id,
        sig: chalk_ir::FnSig { abi: (), safety: chalk_ir::Safety::Safe, variadic: sig.is_varargs },
        binders: chalk_ir::Binders::new(binders, bound),
    };
    Arc::new(datum)
}

pub(crate) fn fn_def_variance_query(
    db: &dyn HirDatabase,
    _krate: CrateId,
    fn_def_id: FnDefId,
) -> Variances {
    let callable_def: CallableDefId = from_chalk(db, fn_def_id);
    let generic_params = generics(db.upcast(), callable_def.into());
    Variances::from_iter(
        &Interner,
        std::iter::repeat(chalk_ir::Variance::Invariant).take(generic_params.len()),
    )
}

pub(crate) fn adt_variance_query(
    db: &dyn HirDatabase,
    _krate: CrateId,
    chalk_ir::AdtId(adt_id): AdtId,
) -> Variances {
    let generic_params = generics(db.upcast(), adt_id.into());
    Variances::from_iter(
        &Interner,
        std::iter::repeat(chalk_ir::Variance::Invariant).take(generic_params.len()),
    )
}

impl From<FnDefId> for crate::db::InternedCallableDefId {
    fn from(fn_def_id: FnDefId) -> Self {
        InternKey::from_intern_id(fn_def_id.0)
    }
}

impl From<crate::db::InternedCallableDefId> for FnDefId {
    fn from(callable_def_id: crate::db::InternedCallableDefId) -> Self {
        chalk_ir::FnDefId(callable_def_id.as_intern_id())
    }
}

impl From<OpaqueTyId> for crate::db::InternedOpaqueTyId {
    fn from(id: OpaqueTyId) -> Self {
        InternKey::from_intern_id(id.0)
    }
}

impl From<crate::db::InternedOpaqueTyId> for OpaqueTyId {
    fn from(id: crate::db::InternedOpaqueTyId) -> Self {
        chalk_ir::OpaqueTyId(id.as_intern_id())
    }
}

impl From<chalk_ir::ClosureId<Interner>> for crate::db::InternedClosureId {
    fn from(id: chalk_ir::ClosureId<Interner>) -> Self {
        Self::from_intern_id(id.0)
    }
}

impl From<crate::db::InternedClosureId> for chalk_ir::ClosureId<Interner> {
    fn from(id: crate::db::InternedClosureId) -> Self {
        chalk_ir::ClosureId(id.as_intern_id())
    }
}
