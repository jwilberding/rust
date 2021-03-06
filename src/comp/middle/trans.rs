import std._str;
import std._vec;
import std._str.rustrt.sbuf;
import std._vec.rustrt.vbuf;
import std.map.hashmap;
import std.option;
import std.option.some;
import std.option.none;

import front.ast;
import driver.session;
import middle.typeck;
import back.x86;
import back.abi;

import util.common;
import util.common.istr;
import util.common.new_def_hash;
import util.common.new_str_hash;

import lib.llvm.llvm;
import lib.llvm.builder;
import lib.llvm.llvm.ModuleRef;
import lib.llvm.llvm.ValueRef;
import lib.llvm.llvm.TypeRef;
import lib.llvm.llvm.BuilderRef;
import lib.llvm.llvm.BasicBlockRef;

import lib.llvm.False;
import lib.llvm.True;

state obj namegen(mutable int i) {
    fn next(str prefix) -> str {
        i += 1;
        ret prefix + istr(i);
    }
}

type glue_fns = rec(ValueRef activate_glue,
                    ValueRef yield_glue,
                    ValueRef exit_task_glue,
                    vec[ValueRef] upcall_glues);

state type crate_ctxt = rec(session.session sess,
                            ModuleRef llmod,
                            hashmap[str, ValueRef] upcalls,
                            hashmap[str, ValueRef] intrinsics,
                            hashmap[str, ValueRef] fn_names,
                            hashmap[ast.def_id, ValueRef] fn_ids,
                            hashmap[ast.def_id, @ast.item] items,
                            @glue_fns glues,
                            namegen names,
                            str path);

state type fn_ctxt = rec(ValueRef llfn,
                         ValueRef lltaskptr,
                         hashmap[ast.def_id, ValueRef] llargs,
                         hashmap[ast.def_id, ValueRef] lllocals,
                         @crate_ctxt ccx);

tag cleanup {
    clean(fn(@block_ctxt cx) -> result);
}

state type block_ctxt = rec(BasicBlockRef llbb,
                            builder build,
                            block_parent parent,
                            mutable vec[cleanup] cleanups,
                            @fn_ctxt fcx);

// FIXME: we should be able to use option.t[@block_parent] here but
// the infinite-tag check in rustboot gets upset.

tag block_parent {
    parent_none;
    parent_some(@block_ctxt);
}


state type result = rec(mutable @block_ctxt bcx,
                        mutable ValueRef val);

fn res(@block_ctxt bcx, ValueRef val) -> result {
    ret rec(mutable bcx = bcx,
            mutable val = val);
}

fn ty_str(TypeRef t) -> str {
    ret lib.llvm.type_to_str(t);
}

fn val_ty(ValueRef v) -> TypeRef {
    ret llvm.LLVMTypeOf(v);
}

fn val_str(ValueRef v) -> str {
    ret ty_str(val_ty(v));
}


// LLVM type constructors.

fn T_void() -> TypeRef {
    // Note: For the time being llvm is kinda busted here, it has the notion
    // of a 'void' type that can only occur as part of the signature of a
    // function, but no general unit type of 0-sized value. This is, afaict,
    // vestigial from its C heritage, and we'll be attempting to submit a
    // patch upstream to fix it. In the mean time we only model function
    // outputs (Rust functions and C functions) using T_void, and model the
    // Rust general purpose nil type you can construct as 1-bit (always
    // zero). This makes the result incorrect for now -- things like a tuple
    // of 10 nil values will have 10-bit size -- but it doesn't seem like we
    // have any other options until it's fixed upstream.
    ret llvm.LLVMVoidType();
}

fn T_nil() -> TypeRef {
    // NB: See above in T_void().
    ret llvm.LLVMInt1Type();
}

fn T_i1() -> TypeRef {
    ret llvm.LLVMInt1Type();
}

fn T_i8() -> TypeRef {
    ret llvm.LLVMInt8Type();
}

fn T_i16() -> TypeRef {
    ret llvm.LLVMInt16Type();
}

fn T_i32() -> TypeRef {
    ret llvm.LLVMInt32Type();
}

fn T_i64() -> TypeRef {
    ret llvm.LLVMInt64Type();
}

fn T_f32() -> TypeRef {
    ret llvm.LLVMFloatType();
}

fn T_f64() -> TypeRef {
    ret llvm.LLVMDoubleType();
}

fn T_bool() -> TypeRef {
    ret T_i1();
}

fn T_int() -> TypeRef {
    // FIXME: switch on target type.
    ret T_i32();
}

fn T_char() -> TypeRef {
    ret T_i32();
}

fn T_fn(vec[TypeRef] inputs, TypeRef output) -> TypeRef {
    ret llvm.LLVMFunctionType(output,
                              _vec.buf[TypeRef](inputs),
                              _vec.len[TypeRef](inputs),
                              False);
}

fn T_ptr(TypeRef t) -> TypeRef {
    ret llvm.LLVMPointerType(t, 0u);
}

fn T_struct(vec[TypeRef] elts) -> TypeRef {
    ret llvm.LLVMStructType(_vec.buf[TypeRef](elts),
                            _vec.len[TypeRef](elts),
                            False);
}

fn T_opaque() -> TypeRef {
    ret llvm.LLVMOpaqueType();
}

fn T_task() -> TypeRef {
    ret T_struct(vec(T_int(),      // Refcount
                     T_int(),      // Delegate pointer
                     T_int(),      // Stack segment pointer
                     T_int(),      // Runtime SP
                     T_int(),      // Rust SP
                     T_int(),      // GC chain
                     T_int(),      // Domain pointer
                     T_int()       // Crate cache pointer
                     ));
}

fn T_array(TypeRef t, uint n) -> TypeRef {
    ret llvm.LLVMArrayType(t, n);
}

fn T_vec(TypeRef t) -> TypeRef {
    ret T_struct(vec(T_int(),       // Refcount
                     T_int(),       // Alloc
                     T_int(),       // Fill
                     T_array(t, 0u) // Body elements
                     ));
}

fn T_str() -> TypeRef {
    ret T_vec(T_i8());
}

fn T_box(TypeRef t) -> TypeRef {
    ret T_struct(vec(T_int(), t));
}

fn T_crate() -> TypeRef {
    ret T_struct(vec(T_int(),      // ptrdiff_t image_base_off
                     T_int(),      // uintptr_t self_addr
                     T_int(),      // ptrdiff_t debug_abbrev_off
                     T_int(),      // size_t debug_abbrev_sz
                     T_int(),      // ptrdiff_t debug_info_off
                     T_int(),      // size_t debug_info_sz
                     T_int(),      // size_t activate_glue_off
                     T_int(),      // size_t yield_glue_off
                     T_int(),      // size_t unwind_glue_off
                     T_int(),      // size_t gc_glue_off
                     T_int(),      // size_t main_exit_task_glue_off
                     T_int(),      // int n_rust_syms
                     T_int(),      // int n_c_syms
                     T_int()       // int n_libs
                     ));
}

fn T_double() -> TypeRef {
    ret llvm.LLVMDoubleType();
}

fn T_taskptr() -> TypeRef {
    ret T_ptr(T_task());
}

fn type_of(@crate_ctxt cx, @typeck.ty t) -> TypeRef {
    let TypeRef llty = type_of_inner(cx, t);
    check (llty as int != 0);
    ret llty;
}

fn type_of_inner(@crate_ctxt cx, @typeck.ty t) -> TypeRef {
    alt (t.struct) {
        case (typeck.ty_nil) { ret T_nil(); }
        case (typeck.ty_bool) { ret T_bool(); }
        case (typeck.ty_int) { ret T_int(); }
        case (typeck.ty_uint) { ret T_int(); }
        case (typeck.ty_machine(?tm)) {
            alt (tm) {
                case (common.ty_i8) { ret T_i8(); }
                case (common.ty_u8) { ret T_i8(); }
                case (common.ty_i16) { ret T_i16(); }
                case (common.ty_u16) { ret T_i16(); }
                case (common.ty_i32) { ret T_i32(); }
                case (common.ty_u32) { ret T_i32(); }
                case (common.ty_i64) { ret T_i64(); }
                case (common.ty_u64) { ret T_i64(); }
                case (common.ty_f32) { ret T_f32(); }
                case (common.ty_f64) { ret T_f64(); }
            }
        }
        case (typeck.ty_char) { ret T_char(); }
        case (typeck.ty_str) { ret T_ptr(T_str()); }
        case (typeck.ty_box(?t)) {
            ret T_ptr(T_box(type_of(cx, t)));
        }
        case (typeck.ty_vec(?t)) {
            ret T_ptr(T_vec(type_of(cx, t)));
        }
        case (typeck.ty_tup(?elts)) {
            let vec[TypeRef] tys = vec();
            for (tup(bool, @typeck.ty) elt in elts) {
                tys += type_of(cx, elt._1);
            }
            ret T_struct(tys);
        }
        case (typeck.ty_fn(?args, ?out)) {
            let vec[TypeRef] atys = vec(T_taskptr());
            for (typeck.arg arg in args) {
                let TypeRef t = type_of(cx, arg.ty);
                alt (arg.mode) {
                    case (ast.alias) {
                        t = T_ptr(t);
                    }
                }
                atys += t;
            }
            ret T_fn(atys, type_of(cx, out));
        }
        case (typeck.ty_var(_)) {
            // FIXME: implement.
            log "ty_var in trans.type_of";
            ret T_i8();
        }
    }
    fail;
}

// LLVM constant constructors.

fn C_null(TypeRef t) -> ValueRef {
    ret llvm.LLVMConstNull(t);
}

fn C_integral(int i, TypeRef t) -> ValueRef {
    // FIXME. We can't use LLVM.ULongLong with our existing minimal native
    // API, which only knows word-sized args.  Lucky for us LLVM has a "take a
    // string encoding" version.  Hilarious. Please fix to handle:
    //
    // ret llvm.LLVMConstInt(T_int(), t as LLVM.ULongLong, False);
    //
    ret llvm.LLVMConstIntOfString(t, _str.buf(istr(i)), 10);
}

fn C_nil() -> ValueRef {
    // NB: See comment above in T_void().
    ret C_integral(0, T_i1());
}

fn C_bool(bool b) -> ValueRef {
    if (b) {
        ret C_integral(1, T_bool());
    } else {
        ret C_integral(0, T_bool());
    }
}

fn C_int(int i) -> ValueRef {
    ret C_integral(i, T_int());
}

fn C_str(@crate_ctxt cx, str s) -> ValueRef {
    auto sc = llvm.LLVMConstString(_str.buf(s), _str.byte_len(s), False);
    auto g = llvm.LLVMAddGlobal(cx.llmod, val_ty(sc),
                                _str.buf(cx.names.next("str")));
    llvm.LLVMSetInitializer(g, sc);
    ret g;
}

fn C_struct(vec[ValueRef] elts) -> ValueRef {
    ret llvm.LLVMConstStruct(_vec.buf[ValueRef](elts),
                             _vec.len[ValueRef](elts),
                             False);
}

fn C_tydesc(TypeRef t) -> ValueRef {
    ret C_struct(vec(C_null(T_opaque()),        // first_param
                     llvm.LLVMSizeOf(t),        // size
                     llvm.LLVMAlignOf(t),       // align
                     C_null(T_opaque()),        // copy_glue_off
                     C_null(T_opaque()),        // drop_glue_off
                     C_null(T_opaque()),        // free_glue_off
                     C_null(T_opaque()),        // sever_glue_off
                     C_null(T_opaque()),        // mark_glue_off
                     C_null(T_opaque()),        // obj_drop_glue_off
                     C_null(T_opaque())));      // is_stateful
}

fn decl_fn(ModuleRef llmod, str name, uint cc, TypeRef llty) -> ValueRef {
    let ValueRef llfn =
        llvm.LLVMAddFunction(llmod, _str.buf(name), llty);
    llvm.LLVMSetFunctionCallConv(llfn, cc);
    ret llfn;
}

fn decl_cdecl_fn(ModuleRef llmod, str name, TypeRef llty) -> ValueRef {
    ret decl_fn(llmod, name, lib.llvm.LLVMCCallConv, llty);
}

fn decl_fastcall_fn(ModuleRef llmod, str name, TypeRef llty) -> ValueRef {
    ret decl_fn(llmod, name, lib.llvm.LLVMFastCallConv, llty);
}

fn decl_glue(ModuleRef llmod, str s) -> ValueRef {
    ret decl_cdecl_fn(llmod, s, T_fn(vec(T_taskptr()), T_void()));
}

fn decl_upcall(ModuleRef llmod, uint _n) -> ValueRef {
    // It doesn't actually matter what type we come up with here, at the
    // moment, as we cast the upcall function pointers to int before passing
    // them to the indirect upcall-invocation glue.  But eventually we'd like
    // to call them directly, once we have a calling convention worked out.
    let int n = _n as int;
    let str s = abi.upcall_glue_name(n);
    let vec[TypeRef] args =
        vec(T_taskptr(), // taskptr
            T_int())     // callee
        + _vec.init_elt[TypeRef](T_int(), n as uint);

    ret decl_fastcall_fn(llmod, s, T_fn(args, T_int()));
}

fn get_upcall(@crate_ctxt cx, str name, int n_args) -> ValueRef {
    if (cx.upcalls.contains_key(name)) {
        ret cx.upcalls.get(name);
    }
    auto inputs = vec(T_taskptr());
    inputs += _vec.init_elt[TypeRef](T_int(), n_args as uint);
    auto output = T_int();
    auto f = decl_cdecl_fn(cx.llmod, name, T_fn(inputs, output));
    cx.upcalls.insert(name, f);
    ret f;
}

fn trans_upcall(@block_ctxt cx, str name, vec[ValueRef] args) -> result {
    let int n = _vec.len[ValueRef](args) as int;
    let ValueRef llupcall = get_upcall(cx.fcx.ccx, name, n);
    llupcall = llvm.LLVMConstPointerCast(llupcall, T_int());

    let ValueRef llglue = cx.fcx.ccx.glues.upcall_glues.(n);
    let vec[ValueRef] call_args = vec(cx.fcx.lltaskptr, llupcall);
    for (ValueRef a in args) {
        call_args += cx.build.ZExtOrBitCast(a, T_int());
    }
    ret res(cx, cx.build.FastCall(llglue, call_args));
}

fn trans_non_gc_free(@block_ctxt cx, ValueRef v) -> result {
    ret trans_upcall(cx, "upcall_free", vec(cx.build.PtrToInt(v, T_int()),
                                            C_int(0)));
}

fn incr_refcnt(@block_ctxt cx, ValueRef box_ptr) -> result {
    auto rc_ptr = cx.build.GEP(box_ptr, vec(C_int(0),
                                            C_int(abi.box_rc_field_refcnt)));
    auto rc = cx.build.Load(rc_ptr);

    auto next_cx = new_sub_block_ctxt(cx, "next");
    auto rc_adj_cx = new_sub_block_ctxt(cx, "rc++");

    auto const_test = cx.build.ICmp(lib.llvm.LLVMIntEQ,
                                    C_int(abi.const_refcount as int), rc);
    cx.build.CondBr(const_test, next_cx.llbb, rc_adj_cx.llbb);

    rc = rc_adj_cx.build.Add(rc, C_int(1));
    rc_adj_cx.build.Store(rc, rc_ptr);
    rc_adj_cx.build.Br(next_cx.llbb);

    ret res(next_cx, C_nil());
}

fn decr_refcnt_and_if_zero(@block_ctxt cx,
                           ValueRef box_ptr,
                           fn(@block_ctxt cx) -> result inner,
                           str inner_name,
                           TypeRef t_else, ValueRef v_else) -> result {

    auto rc_adj_cx = new_sub_block_ctxt(cx, "rc--");
    auto inner_cx = new_sub_block_ctxt(cx, inner_name);
    auto next_cx = new_sub_block_ctxt(cx, "next");

    auto rc_ptr = cx.build.GEP(box_ptr, vec(C_int(0),
                                            C_int(abi.box_rc_field_refcnt)));
    auto rc = cx.build.Load(rc_ptr);

    auto const_test = cx.build.ICmp(lib.llvm.LLVMIntEQ,
                                    C_int(abi.const_refcount as int), rc);
    cx.build.CondBr(const_test, next_cx.llbb, rc_adj_cx.llbb);

    rc = rc_adj_cx.build.Sub(rc, C_int(1));
    rc_adj_cx.build.Store(rc, rc_ptr);
    auto zero_test = rc_adj_cx.build.ICmp(lib.llvm.LLVMIntEQ, C_int(0), rc);
    rc_adj_cx.build.CondBr(zero_test, inner_cx.llbb, next_cx.llbb);

    auto inner_res = inner(inner_cx);
    inner_res.bcx.build.Br(next_cx.llbb);

    auto phi = next_cx.build.Phi(t_else,
                                 vec(v_else, v_else, inner_res.val),
                                 vec(cx.llbb,
                                     rc_adj_cx.llbb,
                                     inner_res.bcx.llbb));

    ret res(next_cx, phi);
}

type val_and_ty_fn =
    fn(@block_ctxt cx, ValueRef v, @typeck.ty t) -> result;

// Iterates through the elements of a tup, rec or tag.
fn iter_structural_ty(@block_ctxt cx,
                      ValueRef v,
                      @typeck.ty t,
                      val_and_ty_fn f)
    -> result {
    let result r = res(cx, C_nil());
    alt (t.struct) {
        case (typeck.ty_tup(?args)) {
            let int i = 0;
            for (tup(bool, @typeck.ty) arg in args) {
                auto elt = r.bcx.build.GEP(v, vec(C_int(0), C_int(i)));
                r = f(r.bcx, elt, arg._1);
                i += 1;
            }
        }
        // FIXME: handle records and tags when we support them.
    }
    ret r;
}

// Iterates through the elements of a vec or str.
fn iter_sequence(@block_ctxt cx,
                 ValueRef v,
                 @typeck.ty ty,
                 val_and_ty_fn f) -> result {

    fn iter_sequence_body(@block_ctxt cx,
                          ValueRef v,
                          @typeck.ty elt_ty,
                          val_and_ty_fn f,
                          bool trailing_null) -> result {

        auto p0 = cx.build.GEP(v, vec(C_int(0),
                                      C_int(abi.vec_elt_data)));
        auto lenptr = cx.build.GEP(v, vec(C_int(0),
                                          C_int(abi.vec_elt_fill)));
        auto len = cx.build.Load(lenptr);
        if (trailing_null) {
            len = cx.build.Sub(len, C_int(1));
        }

        auto r = res(cx, C_nil());

        auto cond_cx = new_sub_block_ctxt(cx, "sequence-iter cond");
        auto body_cx = new_sub_block_ctxt(cx, "sequence-iter body");
        auto next_cx = new_sub_block_ctxt(cx, "next");

        auto ix = cond_cx.build.Phi(T_int(), vec(C_int(0)), vec(cx.llbb));
        auto end_test = cond_cx.build.ICmp(lib.llvm.LLVMIntEQ, ix, len);
        cond_cx.build.CondBr(end_test, body_cx.llbb, next_cx.llbb);

        auto elt = body_cx.build.GEP(p0, vec(ix));
        auto body_res = f(body_cx, elt, elt_ty);
        auto next_ix = body_res.bcx.build.Add(ix, C_int(1));
        cond_cx.build.AddIncomingToPhi(ix, vec(next_ix),
                                       vec(body_res.bcx.llbb));

        body_res.bcx.build.Br(cond_cx.llbb);
        ret res(next_cx, C_nil());
    }

    alt (ty.struct) {
        case (typeck.ty_vec(?et)) {
            ret iter_sequence_body(cx, v, et, f, false);
        }
        case (typeck.ty_str) {
            auto et = typeck.plain_ty(typeck.ty_machine(common.ty_u8));
            ret iter_sequence_body(cx, v, et, f, false);
        }
    }
    cx.fcx.ccx.sess.bug("bad type in trans.iter_sequence");
    fail;
}

fn incr_all_refcnts(@block_ctxt cx,
                    ValueRef v,
                    @typeck.ty t) -> result {

    if (typeck.type_is_boxed(t)) {
        ret incr_refcnt(cx, v);

    } else if (typeck.type_is_binding(t)) {
        cx.fcx.ccx.sess.unimpl("binding type in trans.incr_all_refcnts");

    } else if (typeck.type_is_structural(t)) {
        ret iter_structural_ty(cx, v, t,
                               bind incr_all_refcnts(_, _, _));
    }
    ret res(cx, C_nil());
}

fn drop_ty(@block_ctxt cx,
           ValueRef v,
           @typeck.ty t) -> result {

    alt (t.struct) {
        case (typeck.ty_str) {
            ret decr_refcnt_and_if_zero(cx, v,
                                        bind trans_non_gc_free(_, v),
                                        "free string",
                                        T_int(), C_int(0));
        }

        case (typeck.ty_vec(_)) {
            fn hit_zero(@block_ctxt cx, ValueRef v,
                        @typeck.ty t) -> result {
                auto res = iter_sequence(cx, v, t, bind drop_ty(_,_,_));
                // FIXME: switch gc/non-gc on stratum of the type.
                ret trans_non_gc_free(res.bcx, v);
            }
            ret decr_refcnt_and_if_zero(cx, v,
                                        bind hit_zero(_, v, t),
                                        "free vector",
                                        T_int(), C_int(0));
        }

        case (typeck.ty_box(_)) {
            fn hit_zero(@block_ctxt cx, ValueRef v,
                        @typeck.ty elt_ty) -> result {
                auto res = drop_ty(cx,
                                   cx.build.GEP(v, vec(C_int(0))),
                                   elt_ty);
                // FIXME: switch gc/non-gc on stratum of the type.
                ret trans_non_gc_free(res.bcx, v);
            }
            ret incr_refcnt(cx, v);
        }

        case (_) {
            if (typeck.type_is_structural(t)) {
                ret iter_structural_ty(cx, v, t,
                                       bind drop_ty(_, _, _));

            } else if (typeck.type_is_binding(t)) {
                cx.fcx.ccx.sess.unimpl("binding type in trans.drop_ty");

            } else if (typeck.type_is_scalar(t) ||
                       typeck.type_is_nil(t)) {
                ret res(cx, C_nil());
            }
        }
    }
    cx.fcx.ccx.sess.bug("bad type in trans.drop_ty");
    fail;
}

fn build_memcpy(@block_ctxt cx,
                ValueRef dst,
                ValueRef src,
                TypeRef llty) -> result {
    // FIXME: switch to the 64-bit variant when on such a platform.
    check (cx.fcx.ccx.intrinsics.contains_key("llvm.memcpy.p0i8.p0i8.i32"));
    auto memcpy = cx.fcx.ccx.intrinsics.get("llvm.memcpy.p0i8.p0i8.i32");
    auto src_ptr = cx.build.PointerCast(src, T_ptr(T_i8()));
    auto dst_ptr = cx.build.PointerCast(dst, T_ptr(T_i8()));
    auto size = cx.build.IntCast(lib.llvm.llvm.LLVMSizeOf(llty),
                                 T_i32());
    auto align = cx.build.IntCast(C_int(1), T_i32());

    // FIXME: align seems like it should be
    //   lib.llvm.llvm.LLVMAlignOf(llty);
    // but this makes it upset because it's not a constant.

    auto volatile = C_integral(0, T_i1());
    ret res(cx, cx.build.Call(memcpy,
                              vec(dst_ptr, src_ptr,
                                  size, align, volatile)));
}

fn copy_ty(@block_ctxt cx,
           bool is_init,
           ValueRef dst,
           ValueRef src,
           @typeck.ty t) -> result {
    if (typeck.type_is_scalar(t)) {
        ret res(cx, cx.build.Store(src, dst));

    } else if (typeck.type_is_nil(t)) {
        ret res(cx, C_nil());

    } else if (typeck.type_is_binding(t)) {
        cx.fcx.ccx.sess.unimpl("binding type in trans.copy_ty");

    } else if (typeck.type_is_boxed(t)) {
        auto r = incr_refcnt(cx, src);
        if (! is_init) {
            r = drop_ty(r.bcx, dst, t);
        }
        ret res(r.bcx, r.bcx.build.Store(src, dst));

    } else if (typeck.type_is_structural(t)) {
        auto r = incr_all_refcnts(cx, src, t);
        if (! is_init) {
            r = drop_ty(r.bcx, dst, t);
        }
        // In this one surprising case, we do a load/store on
        // structure types. This results in a memcpy. Usually
        // we talk about structures by pointers in this file.
        ret res(r.bcx, r.bcx.build.Store(r.bcx.build.Load(src), dst));
    }

    cx.fcx.ccx.sess.bug("unexpected type in trans.copy_ty: " +
                        typeck.ty_to_str(t));
    fail;
}

fn trans_drop_str(@block_ctxt cx, ValueRef v) -> result {
    ret decr_refcnt_and_if_zero(cx, v,
                                bind trans_non_gc_free(_, v),
                                "free string",
                                T_int(), C_int(0));
}

impure fn trans_lit(@block_ctxt cx, &ast.lit lit) -> result {
    alt (lit.node) {
        case (ast.lit_int(?i)) {
            ret res(cx, C_int(i));
        }
        case (ast.lit_uint(?u)) {
            ret res(cx, C_int(u as int));
        }
        case (ast.lit_mach_int(?tm, ?i)) {
            // FIXME: the entire handling of mach types falls apart
            // if target int width is larger than host, at the moment;
            // re-do the mach-int types using 'big' when that works.
            auto t = T_int();
            alt (tm) {
                case (common.ty_u8) { t =  T_i8(); }
                case (common.ty_u16) { t =  T_i16(); }
                case (common.ty_u32) { t =  T_i32(); }
                case (common.ty_u64) { t =  T_i64(); }

                case (common.ty_i8) { t =  T_i8(); }
                case (common.ty_i16) { t =  T_i16(); }
                case (common.ty_i32) { t =  T_i32(); }
                case (common.ty_i64) { t =  T_i64(); }
                case (_) {
                    cx.fcx.ccx.sess.bug("bad mach int literal type");
                }
            }
            ret res(cx, C_integral(i, t));
        }
        case (ast.lit_char(?c)) {
            ret res(cx, C_integral(c as int, T_char()));
        }
        case (ast.lit_bool(?b)) {
            ret res(cx, C_bool(b));
        }
        case (ast.lit_nil) {
            ret res(cx, C_nil());
        }
        case (ast.lit_str(?s)) {
            auto len = (_str.byte_len(s) as int) + 1;
            auto sub = trans_upcall(cx, "upcall_new_str",
                                    vec(p2i(C_str(cx.fcx.ccx, s)),
                                        C_int(len)));
            sub.val = sub.bcx.build.IntToPtr(sub.val,
                                             T_ptr(T_str()));
            cx.cleanups += vec(clean(bind trans_drop_str(_, sub.val)));
            ret sub;
        }
    }
}

fn target_type(@crate_ctxt cx, @typeck.ty t) -> @typeck.ty {
    alt (t.struct) {
        case (typeck.ty_int) {
            auto tm = typeck.ty_machine(cx.sess.get_targ_cfg().int_type);
            ret @rec(struct=tm with *t);
        }
        case (typeck.ty_uint) {
            auto tm = typeck.ty_machine(cx.sess.get_targ_cfg().uint_type);
            ret @rec(struct=tm with *t);
        }
    }
    ret t;
}

fn node_ann_type(@crate_ctxt cx, &ast.ann a) -> @typeck.ty {
    alt (a) {
        case (ast.ann_none) {
            log "missing type annotation";
            fail;
        }
        case (ast.ann_type(?t)) {
            ret target_type(cx, t);
        }
    }
}

fn node_type(@crate_ctxt cx, &ast.ann a) -> TypeRef {
    ret type_of(cx, node_ann_type(cx, a));
}

impure fn trans_unary(@block_ctxt cx, ast.unop op,
                      @ast.expr e, &ast.ann a) -> result {

    auto sub = trans_expr(cx, e);

    alt (op) {
        case (ast.bitnot) {
            sub.val = cx.build.Not(sub.val);
            ret sub;
        }
        case (ast.not) {
            sub.val = cx.build.Not(sub.val);
            ret sub;
        }
        case (ast.neg) {
            // FIXME: switch by signedness.
            sub.val = cx.build.Neg(sub.val);
            ret sub;
        }
        case (ast.box) {
            auto e_ty = node_type(cx.fcx.ccx, a);
            auto box_ty = T_box(e_ty);
            sub.val = cx.build.Malloc(box_ty);
            auto rc = sub.bcx.build.GEP(sub.val,
                                        vec(C_int(0),
                                            C_int(abi.box_rc_field_refcnt)));
            ret res(sub.bcx, cx.build.Store(C_int(1), rc));
        }
    }
    cx.fcx.ccx.sess.unimpl("expr variant in trans_unary");
    fail;
}

impure fn trans_binary(@block_ctxt cx, ast.binop op,
                       @ast.expr a, @ast.expr b) -> result {

    // First couple cases are lazy:

    alt (op) {
        case (ast.and) {
            // Lazy-eval and
            auto lhs_res = trans_expr(cx, a);

            auto rhs_cx = new_sub_block_ctxt(cx, "rhs");
            auto rhs_res = trans_expr(rhs_cx, b);

            auto lhs_false_cx = new_sub_block_ctxt(cx, "lhs false");
            auto lhs_false_res = res(lhs_false_cx, C_bool(false));

            lhs_res.bcx.build.CondBr(lhs_res.val,
                                     rhs_cx.llbb,
                                     lhs_false_cx.llbb);

            ret join_results(cx, T_bool(),
                             vec(lhs_false_res, rhs_res));
        }

        case (ast.or) {
            // Lazy-eval or
            auto lhs_res = trans_expr(cx, a);

            auto rhs_cx = new_sub_block_ctxt(cx, "rhs");
            auto rhs_res = trans_expr(rhs_cx, b);

            auto lhs_true_cx = new_sub_block_ctxt(cx, "lhs true");
            auto lhs_true_res = res(lhs_true_cx, C_bool(true));

            lhs_res.bcx.build.CondBr(lhs_res.val,
                                     lhs_true_cx.llbb,
                                     rhs_cx.llbb);

            ret join_results(cx, T_bool(),
                             vec(lhs_true_res, rhs_res));
        }
    }

    // Remaining cases are eager:

    auto lhs = trans_expr(cx, a);
    auto sub = trans_expr(lhs.bcx, b);

    alt (op) {
        case (ast.add) {
            sub.val = cx.build.Add(lhs.val, sub.val);
            ret sub;
        }

        case (ast.sub) {
            sub.val = cx.build.Sub(lhs.val, sub.val);
            ret sub;
        }

        case (ast.mul) {
            // FIXME: switch by signedness.
            sub.val = cx.build.Mul(lhs.val, sub.val);
            ret sub;
        }

        case (ast.div) {
            // FIXME: switch by signedness.
            sub.val = cx.build.SDiv(lhs.val, sub.val);
            ret sub;
        }

        case (ast.rem) {
            // FIXME: switch by signedness.
            sub.val = cx.build.SRem(lhs.val, sub.val);
            ret sub;
        }

        case (ast.bitor) {
            sub.val = cx.build.Or(lhs.val, sub.val);
            ret sub;
        }

        case (ast.bitand) {
            sub.val = cx.build.And(lhs.val, sub.val);
            ret sub;
        }

        case (ast.bitxor) {
            sub.val = cx.build.Xor(lhs.val, sub.val);
            ret sub;
        }

        case (ast.lsl) {
            sub.val = cx.build.Shl(lhs.val, sub.val);
            ret sub;
        }

        case (ast.lsr) {
            sub.val = cx.build.LShr(lhs.val, sub.val);
            ret sub;
        }

        case (ast.asr) {
            sub.val = cx.build.AShr(lhs.val, sub.val);
            ret sub;
        }

        case (ast.eq) {
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntEQ, lhs.val, sub.val);
            ret sub;
        }

        case (ast.ne) {
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntNE, lhs.val, sub.val);
            ret sub;
        }

        case (ast.lt) {
            // FIXME: switch by signedness.
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntSLT, lhs.val, sub.val);
            ret sub;
        }

        case (ast.le) {
            // FIXME: switch by signedness.
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntSLE, lhs.val, sub.val);
            ret sub;
        }

        case (ast.ge) {
            // FIXME: switch by signedness.
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntSGE, lhs.val, sub.val);
            ret sub;
        }

        case (ast.gt) {
            // FIXME: switch by signedness.
            sub.val = cx.build.ICmp(lib.llvm.LLVMIntSGT, lhs.val, sub.val);
            ret sub;
        }
    }
    cx.fcx.ccx.sess.unimpl("expr variant in trans_binary");
    fail;
}

fn join_results(@block_ctxt parent_cx,
                TypeRef t,
                vec[result] ins)
    -> result {

    let vec[result] live = vec();
    let vec[ValueRef] vals = vec();
    let vec[BasicBlockRef] bbs = vec();

    for (result r in ins) {
        if (! is_terminated(r.bcx)) {
            live += r;
            vals += r.val;
            bbs += r.bcx.llbb;
        }
    }

    alt (_vec.len[result](live)) {
        case (0u) {
            // No incoming edges are live, so we're in dead-code-land.
            // Arbitrarily pick the first dead edge, since the caller
            // is just going to propagate it outward.
            check (_vec.len[result](ins) >= 1u);
            ret ins.(0);
        }

        case (1u) {
            // Only one incoming edge is live, so we just feed that block
            // onward.
            ret live.(0);
        }
    }

    // We have >1 incoming edges. Make a join block and br+phi them into it.
    auto join_cx = new_sub_block_ctxt(parent_cx, "join");
    for (result r in live) {
        r.bcx.build.Br(join_cx.llbb);
    }
    auto phi = join_cx.build.Phi(t, vals, bbs);
    ret res(join_cx, phi);
}

impure fn trans_if(@block_ctxt cx, @ast.expr cond,
                   &ast.block thn, &option.t[ast.block] els) -> result {

    auto cond_res = trans_expr(cx, cond);

    auto then_cx = new_sub_block_ctxt(cx, "then");
    auto then_res = trans_block(then_cx, thn);

    auto else_cx = new_sub_block_ctxt(cx, "else");
    auto else_res = res(else_cx, C_nil());

    alt (els) {
        case (some[ast.block](?eblk)) {
            else_res = trans_block(else_cx, eblk);
        }
    }

    cond_res.bcx.build.CondBr(cond_res.val,
                              then_res.bcx.llbb,
                              else_res.bcx.llbb);

    // FIXME: use inferred type when available.
    ret join_results(cx, T_nil(),
                     vec(then_res, else_res));
}

impure fn trans_while(@block_ctxt cx, @ast.expr cond,
                      &ast.block body) -> result {

    auto cond_cx = new_sub_block_ctxt(cx, "while cond");
    auto body_cx = new_sub_block_ctxt(cx, "while loop body");
    auto next_cx = new_sub_block_ctxt(cx, "next");

    auto body_res = trans_block(body_cx, body);
    auto cond_res = trans_expr(cond_cx, cond);

    body_res.bcx.build.Br(cond_cx.llbb);
    cond_res.bcx.build.CondBr(cond_res.val,
                              body_cx.llbb,
                              next_cx.llbb);

    cx.build.Br(cond_cx.llbb);
    ret res(next_cx, C_nil());
}

impure fn trans_do_while(@block_ctxt cx, &ast.block body,
                         @ast.expr cond) -> result {

    auto body_cx = new_sub_block_ctxt(cx, "do-while loop body");
    auto next_cx = new_sub_block_ctxt(cx, "next");

    auto body_res = trans_block(body_cx, body);
    auto cond_res = trans_expr(body_res.bcx, cond);

    cond_res.bcx.build.CondBr(cond_res.val,
                              body_cx.llbb,
                              next_cx.llbb);
    cx.build.Br(body_cx.llbb);
    ret res(next_cx, body_res.val);
}

// The additional bool returned indicates whether it's mem (that is
// represented as an alloca or heap, hence needs a 'load' to be used as an
// immediate).

fn trans_name(@block_ctxt cx, &ast.name n, &option.t[ast.def] dopt)
    -> tup(result, bool) {
    alt (dopt) {
        case (some[ast.def](?def)) {
            alt (def) {
                case (ast.def_arg(?did)) {
                    check (cx.fcx.llargs.contains_key(did));
                    ret tup(res(cx, cx.fcx.llargs.get(did)),
                            true);
                }
                case (ast.def_local(?did)) {
                    check (cx.fcx.lllocals.contains_key(did));
                    ret tup(res(cx, cx.fcx.lllocals.get(did)),
                            true);
                }
                case (ast.def_fn(?did)) {
                    check (cx.fcx.ccx.fn_ids.contains_key(did));
                    ret tup(res(cx, cx.fcx.ccx.fn_ids.get(did)),
                            false);
                }
                case (_) {
                    cx.fcx.ccx.sess.unimpl("def variant in trans");
                }
            }
        }
        case (none[ast.def]) {
            cx.fcx.ccx.sess.err("unresolved expr_name in trans");
        }
    }
    fail;
}

fn trans_field(@block_ctxt cx, &ast.span sp, @ast.expr base,
               &ast.ident field, &ast.ann ann) -> tup(result, bool) {
    auto lv = trans_lval(cx, base);
    auto r = lv._0;
    auto ty = typeck.expr_ty(base);
    alt (ty.struct) {
        case (typeck.ty_tup(?fields)) {
            let uint ix = typeck.field_num(cx.fcx.ccx.sess, sp, field);
            auto v = r.bcx.build.GEP(r.val, vec(C_int(0), C_int(ix as int)));
            ret tup(res(r.bcx, v), lv._1);
        }
    }
    cx.fcx.ccx.sess.unimpl("field variant in trans_field");
    fail;
}

fn trans_lval(@block_ctxt cx, @ast.expr e) -> tup(result, bool) {
    alt (e.node) {
        case (ast.expr_name(?n, ?dopt, _)) {
            ret trans_name(cx, n, dopt);
        }
        case (ast.expr_field(?base, ?ident, ?ann)) {
            ret trans_field(cx, e.span, base, ident, ann);
        }
    }
    cx.fcx.ccx.sess.unimpl("expr variant in trans_lval");
    fail;
}

impure fn trans_cast(@block_ctxt cx, @ast.expr e, &ast.ann ann) -> result {
    auto e_res = trans_expr(cx, e);
    auto llsrctype = val_ty(e_res.val);
    auto t = node_ann_type(cx.fcx.ccx, ann);
    auto lldsttype = type_of(cx.fcx.ccx, t);
    if (!typeck.type_is_fp(t)) {
        if (llvm.LLVMGetIntTypeWidth(lldsttype) >
            llvm.LLVMGetIntTypeWidth(llsrctype)) {
            if (typeck.type_is_signed(t)) {
                // Widening signed cast.
                e_res.val =
                    e_res.bcx.build.SExtOrBitCast(e_res.val,
                                                  lldsttype);
            } else {
                // Widening unsigned cast.
                e_res.val =
                    e_res.bcx.build.ZExtOrBitCast(e_res.val,
                                                  lldsttype);
            }
        } else {
            // Narrowing cast.
            e_res.val =
                e_res.bcx.build.TruncOrBitCast(e_res.val,
                                               lldsttype);
        }
    } else {
        cx.fcx.ccx.sess.unimpl("fp cast");
    }
    ret e_res;
}


impure fn trans_args(@block_ctxt cx, &vec[@ast.expr] es)
    -> tup(@block_ctxt, vec[ValueRef]) {
    let vec[ValueRef] vs = vec(cx.fcx.lltaskptr);
    let @block_ctxt bcx = cx;

    for (@ast.expr e in es) {
        auto res = trans_expr(bcx, e);
        // Until here we've been treating structures by pointer;
        // we are now passing it as an arg, so need to load it.
        if (typeck.type_is_structural(typeck.expr_ty(e))) {
            res.val = res.bcx.build.Load(res.val);
        }
        vs += res.val;
        bcx = res.bcx;
    }

    ret tup(bcx, vs);
}

impure fn trans_call(@block_ctxt cx, @ast.expr f,
                     vec[@ast.expr] args) -> result {
    auto f_res = trans_lval(cx, f);
    check (! f_res._1);
    auto args_res = trans_args(f_res._0.bcx, args);
    ret res(args_res._0,
            args_res._0.build.FastCall(f_res._0.val, args_res._1));
}

impure fn trans_tup(@block_ctxt cx, vec[tup(bool, @ast.expr)] args,
                    &ast.ann ann) -> result {
    auto ty = node_type(cx.fcx.ccx, ann);
    auto tup_val = cx.build.Alloca(ty);
    let int i = 0;
    auto r = res(cx, C_nil());
    for (tup(bool, @ast.expr) arg in args) {
        auto t = typeck.expr_ty(arg._1);
        auto src_res = trans_expr(r.bcx, arg._1);
        auto dst_elt = r.bcx.build.GEP(tup_val, vec(C_int(0), C_int(i)));
        // FIXME: calculate copy init-ness in typestate.
        r = copy_ty(src_res.bcx, true, dst_elt, src_res.val, t);
        i += 1;
    }
    ret res(r.bcx, tup_val);
}



impure fn trans_expr(@block_ctxt cx, @ast.expr e) -> result {
    alt (e.node) {
        case (ast.expr_lit(?lit, _)) {
            ret trans_lit(cx, *lit);
        }

        case (ast.expr_unary(?op, ?x, ?ann)) {
            ret trans_unary(cx, op, x, ann);
        }

        case (ast.expr_binary(?op, ?x, ?y, _)) {
            ret trans_binary(cx, op, x, y);
        }

        case (ast.expr_if(?cond, ?thn, ?els, _)) {
            ret trans_if(cx, cond, thn, els);
        }

        case (ast.expr_while(?cond, ?body, _)) {
            ret trans_while(cx, cond, body);
        }

        case (ast.expr_do_while(?body, ?cond, _)) {
            ret trans_do_while(cx, body, cond);
        }

        case (ast.expr_block(?blk, _)) {
            auto sub_cx = new_sub_block_ctxt(cx, "block-expr body");
            auto next_cx = new_sub_block_ctxt(cx, "next");
            auto sub = trans_block(sub_cx, blk);

            cx.build.Br(sub_cx.llbb);
            sub.bcx.build.Br(next_cx.llbb);

            ret res(next_cx, sub.val);
        }

        case (ast.expr_assign(?dst, ?src, ?ann)) {
            auto lhs_res = trans_lval(cx, dst);
            check (lhs_res._1);
            auto rhs_res = trans_expr(lhs_res._0.bcx, src);
            auto t = node_ann_type(cx.fcx.ccx, ann);
            // FIXME: calculate copy init-ness in typestate.
            ret copy_ty(rhs_res.bcx, true, lhs_res._0.val, rhs_res.val, t);
        }

        case (ast.expr_call(?f, ?args, _)) {
            ret trans_call(cx, f, args);
        }

        case (ast.expr_cast(?e, _, ?ann)) {
            ret trans_cast(cx, e, ann);
        }

        case (ast.expr_tup(?args, ?ann)) {
            ret trans_tup(cx, args, ann);
        }

        // lval cases fall through to trans_lval and then
        // possibly load the result (if it's non-structural).

        case (_) {
            auto t = typeck.expr_ty(e);
            auto sub = trans_lval(cx, e);
            if (sub._1 && ! typeck.type_is_structural(t)) {
                ret res(sub._0.bcx, cx.build.Load(sub._0.val));
            } else {
                ret sub._0;
            }
        }
    }
    cx.fcx.ccx.sess.unimpl("expr variant in trans_expr");
    fail;
}

impure fn trans_log(@block_ctxt cx, @ast.expr e) -> result {
    alt (e.node) {
        case (ast.expr_lit(?lit, _)) {
            alt (lit.node) {
                case (ast.lit_str(_)) {
                    auto sub = trans_expr(cx, e);
                    auto v = sub.bcx.build.PtrToInt(sub.val, T_int());
                    ret trans_upcall(sub.bcx,
                                     "upcall_log_str",
                                     vec(v));
                }

                case (_) {
                    auto sub = trans_expr(cx, e);
                    ret trans_upcall(sub.bcx,
                                     "upcall_log_int",
                                     vec(sub.val));
                }
            }
        }

        case (_) {
            auto sub = trans_expr(cx, e);
            ret trans_upcall(sub.bcx, "upcall_log_int", vec(sub.val));
        }
    }
}

impure fn trans_check_expr(@block_ctxt cx, @ast.expr e) -> result {
    auto cond_res = trans_expr(cx, e);

    // FIXME: need pretty-printer.
    auto V_expr_str = p2i(C_str(cx.fcx.ccx, "<expr>"));
    auto V_filename = p2i(C_str(cx.fcx.ccx, e.span.filename));
    auto V_line = e.span.lo.line as int;
    auto args = vec(V_expr_str, V_filename, C_int(V_line));

    auto fail_cx = new_sub_block_ctxt(cx, "fail");
    auto fail_res = trans_upcall(fail_cx, "upcall_fail", args);

    auto next_cx = new_sub_block_ctxt(cx, "next");
    fail_res.bcx.build.Br(next_cx.llbb);
    cond_res.bcx.build.CondBr(cond_res.val,
                              next_cx.llbb,
                              fail_cx.llbb);
    ret res(next_cx, C_nil());
}

impure fn trans_ret(@block_ctxt cx, &option.t[@ast.expr] e) -> result {
    auto r = res(cx, C_nil());
    alt (e) {
        case (some[@ast.expr](?x)) {
            r = trans_expr(cx, x);
        }
    }

    // Run all cleanups and back out.
    let bool more_cleanups = true;
    auto cleanup_cx = cx;
    while (more_cleanups) {
        r.bcx = trans_block_cleanups(r.bcx, cleanup_cx);
        alt (cleanup_cx.parent) {
            case (parent_some(?b)) {
                cleanup_cx = b;
            }
            case (parent_none) {
                more_cleanups = false;
            }
        }
    }

    alt (e) {
        case (some[@ast.expr](_)) {
            r.val = r.bcx.build.Ret(r.val);
            ret r;
        }
    }

    // FIXME: until LLVM has a unit type, we are moving around
    // C_nil values rather than their void type.
    r.val = r.bcx.build.Ret(C_nil());
    ret r;
}

impure fn trans_stmt(@block_ctxt cx, &ast.stmt s) -> result {
    auto sub = res(cx, C_nil());
    alt (s.node) {
        case (ast.stmt_log(?a)) {
            sub.bcx = trans_log(cx, a).bcx;
        }

        case (ast.stmt_check_expr(?a)) {
            sub.bcx = trans_check_expr(cx, a).bcx;
        }

        case (ast.stmt_ret(?e)) {
            sub.bcx = trans_ret(cx, e).bcx;
        }

        case (ast.stmt_expr(?e)) {
            sub.bcx = trans_expr(cx, e).bcx;
        }

        case (ast.stmt_decl(?d)) {
            alt (d.node) {
                case (ast.decl_local(?local)) {
                    alt (local.init) {
                        case (some[@ast.expr](?e)) {
                            check (cx.fcx.lllocals.contains_key(local.id));
                            auto llptr = cx.fcx.lllocals.get(local.id);
                            sub = trans_expr(cx, e);
                            copy_ty(sub.bcx, true, llptr, sub.val,
                                    typeck.expr_ty(e));
                        }
                    }
                }
            }
        }
        case (_) {
            cx.fcx.ccx.sess.unimpl("stmt variant");
        }
    }
    ret sub;
}

fn new_builder(BasicBlockRef llbb, str name) -> builder {
    let BuilderRef llbuild = llvm.LLVMCreateBuilder();
    llvm.LLVMPositionBuilderAtEnd(llbuild, llbb);
    ret builder(llbuild);
}

// You probably don't want to use this one. See the
// next three functions instead.
fn new_block_ctxt(@fn_ctxt cx, block_parent parent,
                  vec[cleanup] cleanups,
                  str name) -> @block_ctxt {
    let BasicBlockRef llbb =
        llvm.LLVMAppendBasicBlock(cx.llfn,
                                  _str.buf(cx.ccx.names.next(name)));

    ret @rec(llbb=llbb,
             build=new_builder(llbb, name),
             parent=parent,
             mutable cleanups=cleanups,
             fcx=cx);
}

// Use this when you're at the top block of a function or the like.
fn new_top_block_ctxt(@fn_ctxt fcx) -> @block_ctxt {
    let vec[cleanup] cleanups = vec();
    ret new_block_ctxt(fcx, parent_none, cleanups, "function top level");

}

// Use this when you're making a block-within-a-block.
fn new_sub_block_ctxt(@block_ctxt bcx, str n) -> @block_ctxt {
    let vec[cleanup] cleanups = vec();
    ret new_block_ctxt(bcx.fcx, parent_some(bcx), cleanups, n);
}


fn trans_block_cleanups(@block_ctxt cx,
                        @block_ctxt cleanup_cx) -> @block_ctxt {
    auto bcx = cx;
    for (cleanup c in cleanup_cx.cleanups) {
        alt (c) {
            case (clean(?cfn)) {
                bcx = cfn(bcx).bcx;
            }
        }
    }
    ret bcx;
}

iter block_locals(&ast.block b) -> @ast.local {
    // FIXME: putting from inside an iter block doesn't work, so we can't
    // use the index here.
    for (@ast.stmt s in b.node.stmts) {
        alt (s.node) {
            case (ast.stmt_decl(?d)) {
                alt (d.node) {
                    case (ast.decl_local(?local)) {
                        put local;
                    }
                }
            }
        }
    }
}

impure fn trans_block(@block_ctxt cx, &ast.block b) -> result {
    auto bcx = cx;

    for each (@ast.local local in block_locals(b)) {
        auto ty = node_type(cx.fcx.ccx, local.ann);
        auto val = bcx.build.Alloca(ty);
        cx.fcx.lllocals.insert(local.id, val);
    }
    auto r = res(bcx, C_nil());

    for (@ast.stmt s in b.node.stmts) {
        r = trans_stmt(bcx, *s);
        bcx = r.bcx;
        // If we hit a terminator, control won't go any further so
        // we're in dead-code land. Stop here.
        if (is_terminated(bcx)) {
            ret r;
        }
    }

    bcx = trans_block_cleanups(bcx, bcx);
    ret res(bcx, r.val);
}

fn new_fn_ctxt(@crate_ctxt cx,
               str name,
               &ast._fn f,
               ast.def_id fid) -> @fn_ctxt {

    check (cx.fn_ids.contains_key(fid));
    let ValueRef llfn = cx.fn_ids.get(fid);
    cx.fn_names.insert(cx.path, llfn);

    let ValueRef lltaskptr = llvm.LLVMGetParam(llfn, 0u);
    let uint arg_n = 1u;

    let hashmap[ast.def_id, ValueRef] lllocals = new_def_hash[ValueRef]();
    let hashmap[ast.def_id, ValueRef] llargs = new_def_hash[ValueRef]();

    for (ast.arg arg in f.inputs) {
        auto llarg = llvm.LLVMGetParam(llfn, arg_n);
        check (llarg as int != 0);
        llargs.insert(arg.id, llarg);
        arg_n += 1u;
    }

    ret @rec(llfn=llfn,
             lltaskptr=lltaskptr,
             llargs=llargs,
             lllocals=lllocals,
             ccx=cx);
}


// Recommended LLVM style, strange though this is, is to copy from args to
// allocas immediately upon entry; this permits us to GEP into structures we
// were passed and whatnot. Apparently mem2reg will mop up.

fn copy_args_to_allocas(@block_ctxt cx, &ast._fn f, &ast.ann ann) {

    let vec[typeck.arg] arg_ts = vec();
    let @typeck.ty fty = node_ann_type(cx.fcx.ccx, ann);
    alt (fty.struct) {
        case (typeck.ty_fn(?a, _)) { arg_ts += a; }
    }

    let uint arg_n = 0u;

    for (ast.arg aarg in f.inputs) {
        auto arg = arg_ts.(arg_n);
        auto arg_t = type_of(cx.fcx.ccx, arg.ty);
        auto alloca = cx.build.Alloca(arg_t);
        auto argval = cx.fcx.llargs.get(aarg.id);
        cx.build.Store(argval, alloca);
        // Overwrite the llargs entry for this arg with its alloca.
        cx.fcx.llargs.insert(aarg.id, alloca);
        arg_n += 1u;
    }
}

fn is_terminated(@block_ctxt cx) -> bool {
    auto inst = llvm.LLVMGetLastInstruction(cx.llbb);
    ret llvm.LLVMIsATerminatorInst(inst) as int != 0;
}

impure fn trans_fn(@crate_ctxt cx, &ast._fn f, ast.def_id fid,
                   &ast.ann ann) {

    auto fcx = new_fn_ctxt(cx, cx.path, f, fid);
    auto bcx = new_top_block_ctxt(fcx);

    copy_args_to_allocas(bcx, f, ann);

    auto res = trans_block(bcx, f.body);
    if (!is_terminated(res.bcx)) {
        // FIXME: until LLVM has a unit type, we are moving around
        // C_nil values rather than their void type.
        res.bcx.build.Ret(C_nil());
    }
}

impure fn trans_item(@crate_ctxt cx, &ast.item item) {
    alt (item.node) {
        case (ast.item_fn(?name, ?f, _, ?fid, ?ann)) {
            auto sub_cx = @rec(path=cx.path + "." + name with *cx);
            trans_fn(sub_cx, f, fid, ann);
        }
        case (ast.item_mod(?name, ?m, _)) {
            auto sub_cx = @rec(path=cx.path + "." + name with *cx);
            trans_mod(sub_cx, m);
        }
    }
}

impure fn trans_mod(@crate_ctxt cx, &ast._mod m) {
    for (@ast.item item in m.items) {
        trans_item(cx, *item);
    }
}


fn collect_item(&@crate_ctxt cx, @ast.item i) -> @crate_ctxt {
    alt (i.node) {
        case (ast.item_fn(?name, ?f, _, ?fid, ?ann)) {
            // TODO: type-params
            cx.items.insert(fid, i);
            auto llty = node_type(cx, ann);
            let str s = cx.names.next("_rust_fn") + "." + name;
            let ValueRef llfn = decl_fastcall_fn(cx.llmod, s, llty);
            cx.fn_ids.insert(fid, llfn);
        }

        case (ast.item_mod(?name, ?m, ?mid)) {
            cx.items.insert(mid, i);
        }
    }
    ret cx;
}


fn collect_items(@crate_ctxt cx, @ast.crate crate) {

    let fold.ast_fold[@crate_ctxt] fld =
        fold.new_identity_fold[@crate_ctxt]();

    fld = @rec( update_env_for_item = bind collect_item(_,_)
                with *fld );

    fold.fold_crate[@crate_ctxt](cx, fld, crate);
}

fn p2i(ValueRef v) -> ValueRef {
    ret llvm.LLVMConstPtrToInt(v, T_int());
}

fn trans_exit_task_glue(@crate_ctxt cx) {
    let vec[TypeRef] T_args = vec();
    let vec[ValueRef] V_args = vec();

    auto llfn = cx.glues.exit_task_glue;
    let ValueRef lltaskptr = llvm.LLVMGetParam(llfn, 0u);
    auto fcx = @rec(llfn=llfn,
                    lltaskptr=lltaskptr,
                    llargs=new_def_hash[ValueRef](),
                    lllocals=new_def_hash[ValueRef](),
                    ccx=cx);

    auto bcx = new_top_block_ctxt(fcx);
    trans_upcall(bcx, "upcall_exit", V_args);
    bcx.build.RetVoid();
}

fn crate_constant(@crate_ctxt cx) -> ValueRef {

    let ValueRef crate_ptr =
        llvm.LLVMAddGlobal(cx.llmod, T_crate(),
                           _str.buf("rust_crate"));

    let ValueRef crate_addr = p2i(crate_ptr);

    let ValueRef activate_glue_off =
        llvm.LLVMConstSub(p2i(cx.glues.activate_glue), crate_addr);

    let ValueRef yield_glue_off =
        llvm.LLVMConstSub(p2i(cx.glues.yield_glue), crate_addr);

    let ValueRef exit_task_glue_off =
        llvm.LLVMConstSub(p2i(cx.glues.exit_task_glue), crate_addr);

    let ValueRef crate_val =
        C_struct(vec(C_null(T_int()),     // ptrdiff_t image_base_off
                     p2i(crate_ptr),      // uintptr_t self_addr
                     C_null(T_int()),     // ptrdiff_t debug_abbrev_off
                     C_null(T_int()),     // size_t debug_abbrev_sz
                     C_null(T_int()),     // ptrdiff_t debug_info_off
                     C_null(T_int()),     // size_t debug_info_sz
                     activate_glue_off,   // size_t activate_glue_off
                     yield_glue_off,      // size_t yield_glue_off
                     C_null(T_int()),     // size_t unwind_glue_off
                     C_null(T_int()),     // size_t gc_glue_off
                     exit_task_glue_off,  // size_t main_exit_task_glue_off
                     C_null(T_int()),     // int n_rust_syms
                     C_null(T_int()),     // int n_c_syms
                     C_null(T_int())      // int n_libs
                     ));

    llvm.LLVMSetInitializer(crate_ptr, crate_val);
    ret crate_ptr;
}

fn trans_main_fn(@crate_ctxt cx, ValueRef llcrate) {
    auto T_main_args = vec(T_int(), T_int());
    auto T_rust_start_args = vec(T_int(), T_int(), T_int(), T_int());

    auto main_name;
    if (_str.eq(std.os.target_os(), "win32")) {
        main_name = "WinMain@16";
    } else {
        main_name = "main";
    }

    auto llmain =
        decl_cdecl_fn(cx.llmod, main_name, T_fn(T_main_args, T_int()));

    auto llrust_start = decl_cdecl_fn(cx.llmod, "rust_start",
                                      T_fn(T_rust_start_args, T_int()));

    auto llargc = llvm.LLVMGetParam(llmain, 0u);
    auto llargv = llvm.LLVMGetParam(llmain, 1u);
    check (cx.fn_names.contains_key("_rust.main"));
    auto llrust_main = cx.fn_names.get("_rust.main");

    //
    // Emit the moral equivalent of:
    //
    // main(int argc, char **argv) {
    //     rust_start(&_rust.main, &crate, argc, argv);
    // }
    //

    let BasicBlockRef llbb =
        llvm.LLVMAppendBasicBlock(llmain, _str.buf(""));
    auto b = new_builder(llbb, "");

    auto start_args = vec(p2i(llrust_main), p2i(llcrate), llargc, llargv);

    b.Ret(b.Call(llrust_start, start_args));

}

fn declare_intrinsics(ModuleRef llmod) -> hashmap[str,ValueRef] {

    let vec[TypeRef] T_trap_args = vec();
    let vec[TypeRef] T_memcpy32_args = vec(T_ptr(T_i8()), T_ptr(T_i8()),
                                           T_i32(), T_i32(), T_i1());
    let vec[TypeRef] T_memcpy64_args = vec(T_ptr(T_i8()), T_ptr(T_i8()),
                                           T_i32(), T_i32(), T_i1());
    auto trap = decl_cdecl_fn(llmod, "llvm.trap",
                              T_fn(T_trap_args, T_void()));
    auto memcpy32 = decl_cdecl_fn(llmod, "llvm.memcpy.p0i8.p0i8.i32",
                                  T_fn(T_memcpy32_args, T_void()));
    auto memcpy64 = decl_cdecl_fn(llmod, "llvm.memcpy.p0i8.p0i8.i64",
                                  T_fn(T_memcpy64_args, T_void()));

    auto intrinsics = new_str_hash[ValueRef]();
    intrinsics.insert("llvm.trap", trap);
    intrinsics.insert("llvm.memcpy.p0i8.p0i8.i32", memcpy32);
    intrinsics.insert("llvm.memcpy.p0i8.p0i8.i64", memcpy64);
    ret intrinsics;
}

fn trans_crate(session.session sess, @ast.crate crate, str output) {
    auto llmod =
        llvm.LLVMModuleCreateWithNameInContext(_str.buf("rust_out"),
                                               llvm.LLVMGetGlobalContext());

    llvm.LLVMSetModuleInlineAsm(llmod, _str.buf(x86.get_module_asm()));

    auto intrinsics = declare_intrinsics(llmod);

    auto glues = @rec(activate_glue = decl_glue(llmod,
                                                abi.activate_glue_name()),
                      yield_glue = decl_glue(llmod, abi.yield_glue_name()),
                      /*
                       * Note: the signature passed to decl_cdecl_fn here
                       * looks unusual because it is. It corresponds neither
                       * to an upcall signature nor a normal rust-ABI
                       * signature. In fact it is a fake signature, that
                       * exists solely to acquire the task pointer as an
                       * argument to the upcall. It so happens that the
                       * runtime sets up the task pointer as the sole incoming
                       * argument to the frame that we return into when
                       * returning to the exit task glue. So this is the
                       * signature required to retrieve it.
                       */
                      exit_task_glue =
                      decl_cdecl_fn(llmod, abi.exit_task_glue_name(),
                                    T_fn(vec(T_taskptr()), T_void())),

                      upcall_glues =
                      _vec.init_fn[ValueRef](bind decl_upcall(llmod, _),
                                             abi.n_upcall_glues as uint));

    auto cx = @rec(sess = sess,
                   llmod = llmod,
                   upcalls = new_str_hash[ValueRef](),
                   intrinsics = intrinsics,
                   fn_names = new_str_hash[ValueRef](),
                   fn_ids = new_def_hash[ValueRef](),
                   items = new_def_hash[@ast.item](),
                   glues = glues,
                   names = namegen(0),
                   path = "_rust");

    collect_items(cx, crate);
    trans_mod(cx, crate.node.module);
    trans_exit_task_glue(cx);
    trans_main_fn(cx, crate_constant(cx));

    llvm.LLVMWriteBitcodeToFile(llmod, _str.buf(output));
    llvm.LLVMDisposeModule(llmod);
}

//
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// compile-command: "make -k -C ../.. 2>&1 | sed -e 's/\\/x\\//x:\\//g'";
// End:
//
