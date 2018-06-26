extern crate rustc_target;

use syntax::ast::{IntTy, UintTy};
use self::rustc_target::spec::{HasTargetSpec, Target};

use cretonne_module::{Module, Linkage, FuncId};

use prelude::*;

pub type CurrentBackend = ::cretonne_simplejit::SimpleJITBackend;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Variable(Local);

impl EntityRef for Variable {
    fn new(u: usize) -> Self {
        Variable(Local::new(u))
    }

    fn index(self) -> usize {
        self.0.index()
    }
}

fn cton_type_from_ty(ty: Ty) -> Option<types::Type> {
    Some(match ty.sty {
        TypeVariants::TyBool => types::I8,
        TypeVariants::TyUint(size) => {
            match size {
                UintTy::U8 => types::I8,
                UintTy::U16 => types::I16,
                UintTy::U32 => types::I32,
                UintTy::U64 => types::I64,
                UintTy::U128 => types::I64X2,
                UintTy::Usize => types::I64,
            }
        }
        TypeVariants::TyInt(size) => {
            match size {
                IntTy::I8 => types::I8,
                IntTy::I16 => types::I16,
                IntTy::I32 => types::I32,
                IntTy::I64 => types::I64,
                IntTy::I128 => types::I64X2,
                IntTy::Isize => types::I64,
            }
        }
        TypeVariants::TyFnPtr(_) => types::I64,
        TypeVariants::TyRef(..) | TypeVariants::TyRawPtr(..) => types::I64,
        _ => return None,
    })
}

// FIXME(cretonne) fix types smaller than I32
pub fn fixup_cton_ty(ty: Type) -> Type {
    match ty {
        types::I64X2 | types::I64 | types::I32 => ty,
        _ => types::I32,
    }
}

// FIXME(cretonne) fix load.i8
fn load_workaround(fx: &mut FunctionCx, ty: Type, addr: Value, offset: i32) -> Value {
    use cretonne::codegen::ir::types::*;
    match ty {
        I8 => fx.bcx.ins().uload8(I32, MemFlags::new(), addr, offset),
        I16 => fx.bcx.ins().uload16(I32, MemFlags::new(), addr, offset),
        // I32 and I64 work
        _ => fx.bcx.ins().load(ty, MemFlags::new(), addr, offset),
    }
}

// FIXME(cretonne) fix store.i8
fn store_workaround(fx: &mut FunctionCx, ty: Type, addr: Value, val: Value, offset: i32) {
    use cretonne::codegen::ir::types::*;
    match ty {
        I8 => fx.bcx.ins().istore8(MemFlags::new(), val, addr, offset),
        I16 => fx.bcx.ins().istore16(MemFlags::new(), val, addr, offset),
        // I32 and I64 work
        _ => fx.bcx.ins().store(MemFlags::new(), val, addr, offset),
    };
}

#[derive(Debug, Copy, Clone)]
pub enum CValue<'tcx> {
    ByRef(Value, TyLayout<'tcx>),
    ByVal(Value, TyLayout<'tcx>),
    Func(FuncRef, TyLayout<'tcx>),
}

impl<'tcx> CValue<'tcx> {
    pub fn layout(&self) -> TyLayout<'tcx> {
        match *self {
            CValue::ByRef(_, layout) |
            CValue::ByVal(_, layout) |
            CValue::Func(_, layout) => layout
        }
    }

    pub fn force_stack<'a>(self, fx: &mut FunctionCx<'a, 'tcx>) -> Value where 'tcx: 'a {
        match self {
            CValue::ByRef(value, _layout) => value,
            CValue::ByVal(value, layout) => {
                let stack_slot = fx.bcx.create_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size: layout.size.bytes() as u32,
                    offset: None,
                });
                fx.bcx.ins().stack_store(value, stack_slot, 0);
                fx.bcx.ins().stack_addr(types::I64, stack_slot, 0)
            }
            CValue::Func(func, ty) => {
                let func = fx.bcx.ins().func_addr(types::I64, func);
                CValue::ByVal(func, ty).force_stack(fx)
            }
        }
    }

    pub fn load_value<'a>(self, fx: &mut FunctionCx<'a, 'tcx>) -> Value where 'tcx: 'a{
        match self {
            CValue::ByRef(value, layout) => {
                let cton_ty = fx.cton_type(layout.ty).expect(&format!("{:?}", layout.ty));
                load_workaround(fx, cton_ty, value, 0)
            }
            CValue::ByVal(value, _layout) => value,
            CValue::Func(func, _layout) => {
                fx.bcx.ins().func_addr(types::I64, func)
            }
        }
    }

    pub fn expect_byref(self) -> (Value, TyLayout<'tcx>) {
        match self {
            CValue::ByRef(value, layout) => (value, layout),
            CValue::ByVal(_, _) => bug!("Expected CValue::ByRef, found CValue::ByVal"),
            CValue::Func(_, _) => bug!("Expected CValue::ByRef, found CValue::Func"),
        }
    }

    pub fn value_field<'a>(self, fx: &mut FunctionCx<'a, 'tcx>, field: mir::Field) -> CValue<'tcx> where 'tcx: 'a {
        use rustc::ty::util::IntTypeExt;

        let (base, layout) = match self {
            CValue::ByRef(addr, layout) => (addr, layout),
            _ => bug!("place_field for {:?}", self),
        };
        let field_offset = layout.fields.offset(field.index());
        let field_layout = if field.index() == 0 {
            fx.layout_of(if let ty::TyAdt(adt_def, _) = layout.ty.sty {
                adt_def.repr.discr_type().to_ty(fx.tcx)
            } else {
                // This can only be `0`, for now, so `u8` will suffice.
                fx.tcx.types.u8
            })
        } else {
            layout.field(&*fx, field.index())
        };
        if field_offset.bytes() > 0 {
            let field_offset = fx.bcx.ins().iconst(types::I64, field_offset.bytes() as i64);
            CValue::ByRef(fx.bcx.ins().iadd(base, field_offset), field_layout)
        } else {
            CValue::ByRef(base, field_layout)
        }
    }

    pub fn const_val<'a>(fx: &mut FunctionCx<'a, 'tcx>, ty: Ty<'tcx>, const_val: i64) -> CValue<'tcx> where 'tcx: 'a {
        let cton_ty = fx.cton_type(ty).unwrap();
        let layout = fx.layout_of(ty);
        CValue::ByVal(fx.bcx.ins().iconst(cton_ty, const_val), layout)
    }

    pub fn unchecked_cast_to(self, layout: TyLayout<'tcx>) -> Self {
        match self {
            CValue::ByRef(addr, _) => CValue::ByRef(addr, layout),
            CValue::ByVal(val, _) => CValue::ByVal(val, layout),
            CValue::Func(fun, _) => CValue::Func(fun, layout),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum CPlace<'tcx> {
    Var(Variable, TyLayout<'tcx>),
    Addr(Value, TyLayout<'tcx>),
}

impl<'a, 'tcx: 'a> CPlace<'tcx> {
    pub fn layout(&self) -> TyLayout<'tcx> {
        match *self {
            CPlace::Var(_, layout) |
            CPlace::Addr(_, layout) => layout
        }
    }

    pub fn from_stack_slot(fx: &mut FunctionCx<'a, 'tcx>, stack_slot: StackSlot, ty: Ty<'tcx>) -> CPlace<'tcx> {
        let layout = fx.layout_of(ty);
        CPlace::Addr(fx.bcx.ins().stack_addr(types::I64, stack_slot, 0), layout)
    }

    pub fn to_cvalue(self, fx: &mut FunctionCx<'a, 'tcx>) -> CValue<'tcx> {
        match self {
            CPlace::Var(var, layout) => CValue::ByVal(fx.bcx.use_var(var), layout),
            CPlace::Addr(addr, layout) => CValue::ByRef(addr, layout),
        }
    }

    pub fn expect_addr(self) -> Value {
        match self {
            CPlace::Addr(addr, _layout) => addr,
            CPlace::Var(_, _) => bug!("Expected CPlace::Addr, found CPlace::Var"),
        }
    }

    pub fn write_cvalue(self, fx: &mut FunctionCx<'a, 'tcx>, from: CValue<'tcx>) {
        assert_eq!(self.layout().ty, from.layout().ty, "Can't write value of incompatible type to place");

        match self {
            CPlace::Var(var, _) => {
                let data = from.load_value(fx);
                fx.bcx.def_var(var, data)
            },
            CPlace::Addr(addr, layout) => {
                let size = layout.size.bytes() as i32;

                if let Some(cton_ty) = fx.cton_type(layout.ty) {
                    let data = from.load_value(fx);
                    store_workaround(fx, cton_ty, addr, data, 0);
                } else {
                    for i in 0..size {
                        let from = from.expect_byref();
                        let byte = load_workaround(fx, types::I8, from.0, i);
                        store_workaround(fx, types::I8, addr, byte, i);
                    }
                }
            }
        }
    }

    pub fn place_field(self, fx: &mut FunctionCx<'a, 'tcx>, field: mir::Field) -> CPlace<'tcx> {
        let base = self.expect_addr();
        let layout = self.layout();
        let field_offset = layout.fields.offset(field.index());
        let field_ty = layout.field(&*fx, field.index());
        if field_offset.bytes() > 0 {
            let field_offset = fx.bcx.ins().iconst(types::I64, field_offset.bytes() as i64);
            CPlace::Addr(fx.bcx.ins().iadd(base, field_offset), field_ty)
        } else {
            CPlace::Addr(base, field_ty)
        }
    }

    pub fn unchecked_cast_to(self, layout: TyLayout<'tcx>) -> Self {
        match self {
            CPlace::Var(var, _) => CPlace::Var(var, layout),
            CPlace::Addr(addr, _) => CPlace::Addr(addr, layout),
        }
    }

    pub fn downcast_variant(self, fx: &FunctionCx<'a, 'tcx>, variant: usize) -> Self {
        let layout = self.layout().for_variant(fx, variant);
        self.unchecked_cast_to(layout)
    }
}

pub fn cton_sig_from_fn_sig<'a, 'tcx: 'a>(tcx: TyCtxt<'a, 'tcx, 'tcx>, sig: PolyFnSig<'tcx>, substs: &Substs<'tcx>) -> Signature {
    let sig = tcx.subst_and_normalize_erasing_regions(substs, ParamEnv::reveal_all(), &sig);
    cton_sig_from_mono_fn_sig(sig)
}

pub fn cton_sig_from_instance<'a, 'tcx: 'a>(tcx: TyCtxt<'a, 'tcx, 'tcx>, inst: Instance<'tcx>) -> Signature {
    let fn_ty = inst.ty(tcx);
    let sig = fn_ty.fn_sig(tcx);
    cton_sig_from_mono_fn_sig(sig)
}

pub fn cton_sig_from_mono_fn_sig<'a ,'tcx: 'a>(sig: PolyFnSig<'tcx>) -> Signature {
    let sig = sig.skip_binder();
    let inputs = sig.inputs();
    let _output = sig.output();
    assert!(!sig.variadic, "Variadic function are not yet supported");
    let call_conv = match sig.abi {
        _ => CallConv::SystemV,
    };
    Signature {
        params: Some(types::I64).into_iter() // First param is place to put return val
            .chain(inputs.into_iter().map(|ty| fixup_cton_ty(cton_type_from_ty(ty).unwrap_or(types::I64))))
            .map(AbiParam::new).collect(),
        returns: vec![],
        call_conv,
        argument_bytes: None,
    }
}

pub fn cton_intcast<'a, 'tcx: 'a>(fx: &mut FunctionCx<'a, 'tcx>, val: Value, from: Ty<'tcx>, to: Ty<'tcx>, signed: bool) -> Value {
    let from = fx.cton_type(from).unwrap();
    let to = fx.cton_type(to).unwrap();
    if from == to {
        return val;
    }
    if from.wider_or_equal(to) {
        if signed {
            fx.bcx.ins().sextend(to, val)
        } else {
            fx.bcx.ins().uextend(to, val)
        }
    } else {
        fx.bcx.ins().ireduce(to, val)
    }
}

pub struct FunctionCx<'a, 'tcx: 'a> {
    pub tcx: TyCtxt<'a, 'tcx, 'tcx>,
    pub module: &'a mut Module<CurrentBackend>,
    pub def_id_fn_id_map: &'a mut HashMap<Instance<'tcx>, FuncId>,
    pub instance: Instance<'tcx>,
    pub mir: &'tcx Mir<'tcx>,
    pub param_substs: &'tcx Substs<'tcx>,
    pub bcx: FunctionBuilder<'a, Variable>,
    pub ebb_map: HashMap<BasicBlock, Ebb>,
    pub local_map: HashMap<Local, CPlace<'tcx>>,
}

impl<'a, 'tcx: 'a> LayoutOf for &'a FunctionCx<'a, 'tcx> {
    type Ty = Ty<'tcx>;
    type TyLayout = TyLayout<'tcx>;

    fn layout_of(self, ty: Ty<'tcx>) -> TyLayout<'tcx> {
        let ty = self.monomorphize(&ty);
        self.tcx.layout_of(ParamEnv::reveal_all().and(&ty)).unwrap()
    }
}

impl<'a, 'tcx> layout::HasTyCtxt<'tcx> for &'a FunctionCx<'a, 'tcx> {
    fn tcx<'b>(&'b self) -> TyCtxt<'b, 'tcx, 'tcx> {
        self.tcx
    }
}

impl<'a, 'tcx> layout::HasDataLayout for &'a FunctionCx<'a, 'tcx> {
    fn data_layout(&self) -> &layout::TargetDataLayout {
        &self.tcx.data_layout
    }
}

impl<'a, 'tcx> HasTargetSpec for &'a FunctionCx<'a, 'tcx> {
    fn target_spec(&self) -> &Target {
        &self.tcx.sess.target.target
    }
}

impl<'a, 'tcx: 'a> FunctionCx<'a, 'tcx> {
    pub fn monomorphize<T>(&self, value: &T) -> T
        where T: TypeFoldable<'tcx>
    {
        self.tcx.subst_and_normalize_erasing_regions(
            self.param_substs,
            ty::ParamEnv::reveal_all(),
            value,
        )
    }

    pub fn cton_type(&self, ty: Ty<'tcx>) -> Option<Type> {
        cton_type_from_ty(self.monomorphize(&ty))
    }

    pub fn get_ebb(&self, bb: BasicBlock) -> Ebb {
        *self.ebb_map.get(&bb).unwrap()
    }

    pub fn get_local_place(&mut self, local: Local) -> CPlace<'tcx> {
        *self.local_map.get(&local).unwrap()
    }

    pub fn get_function_ref(&mut self, inst: Instance<'tcx>) -> FuncRef {
        let tcx = self.tcx;
        let module = &mut self.module;
        let func_id = *self.def_id_fn_id_map.entry(inst).or_insert_with(|| {
            let sig = cton_sig_from_instance(tcx, inst);
            module.declare_function(&tcx.absolute_item_path_str(inst.def_id()), Linkage::Local, &sig).unwrap()
        });
        module.declare_func_in_func(func_id, &mut self.bcx.func)
    }
}