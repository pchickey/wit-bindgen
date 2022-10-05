mod component_type_object;

use heck::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::mem;
use wit_bindgen_core::wit_parser::abi::{
    AbiVariant, Bindgen, Bitcast, Instruction, LiftLower, WasmType,
};
use wit_bindgen_core::{uwrite, uwriteln, wit_parser::*, Direction, Files, Generator, Ns};

#[derive(Default)]
pub struct C {
    src: Source,
    in_import: bool,
    opts: Opts,
    funcs: HashMap<String, Vec<Func>>,
    return_pointer_area_size: usize,
    return_pointer_area_align: usize,
    sizes: SizeAlign,
    names: Ns,

    // The set of types that are considered public (aka need to be in the
    // header file) which are anonymous and we're effectively monomorphizing.
    // This is discovered lazily when printing type names.
    public_anonymous_types: BTreeSet<TypeId>,

    // This is similar to `public_anonymous_types` where it's discovered
    // lazily, but the set here are for private types only used in the
    // implementation of functions. These types go in the implementation file,
    // not the header file.
    private_anonymous_types: BTreeSet<TypeId>,

    // Type definitions for the given `TypeId`. This is printed topologically
    // at the end.
    types: HashMap<TypeId, wit_bindgen_core::Source>,

    needs_string: bool,

    direction: Direction,
}

struct Func {
    src: Source,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "clap", derive(clap::Args))]
pub struct Opts {
    // ...
}

impl Opts {
    pub fn build(&self) -> C {
        let mut r = C::new();
        r.opts = self.clone();
        r
    }
}

#[derive(Debug)]
struct Return {
    return_multiple: bool,
    scalar: Option<Scalar>,
    retptrs: Vec<Type>,
}

struct CSig {
    name: String,
    sig: String,
    params: Vec<(bool, String)>,
    ret: Return,
    retptrs: Vec<String>,
}

#[derive(Debug)]
enum Scalar {
    Void,
    OptionBool(Type),
    ResultEnum { err: TypeId, max_err: usize },
    Type(Type),
}

impl C {
    pub fn new() -> C {
        C::default()
    }

    fn abi_variant(dir: Direction) -> AbiVariant {
        // This generator uses the obvious direction to ABI variant mapping.
        match dir {
            Direction::Export => AbiVariant::GuestExport,
            Direction::Import => AbiVariant::GuestImport,
        }
    }

    fn classify_ret(&mut self, iface: &Interface, func: &Function) -> Return {
        let mut ret = Return {
            return_multiple: false,
            scalar: None,
            retptrs: Vec::new(),
        };
        match func.results.len() {
            0 => ret.scalar = Some(Scalar::Void),
            1 => {
                let ty = func.results.iter_types().next().unwrap();
                ret.return_single(iface, ty, ty);
            }
            _ => {
                ret.return_multiple = true;
                ret.retptrs.extend(func.results.iter_types().cloned());
            }
        }
        return ret;
    }

    fn print_sig(&mut self, iface: &Interface, func: &Function) -> CSig {
        let name = format!(
            "{}_{}",
            iface.name.to_snake_case(),
            func.name.to_snake_case()
        );
        self.names.insert(&name).expect("duplicate symbols");
        let start = self.src.h.len();

        let ret = self.classify_ret(iface, func);
        match &ret.scalar {
            None | Some(Scalar::Void) => self.src.h("void"),
            Some(Scalar::OptionBool(_id)) => self.src.h("bool"),
            Some(Scalar::ResultEnum { err, .. }) => self.print_ty(iface, &Type::Id(*err)),
            Some(Scalar::Type(ty)) => self.print_ty(iface, ty),
        }
        self.src.h(" ");
        self.src.h(&name);
        self.src.h("(");
        let mut params = Vec::new();
        for (i, (name, ty)) in func.params.iter().enumerate() {
            if i > 0 {
                self.src.h(", ");
            }
            self.print_ty(iface, ty);
            self.src.h(" ");
            let pointer = self.is_arg_by_pointer(iface, ty);
            if pointer {
                self.src.h("*");
            }
            let name = name.to_snake_case();
            self.src.h(&name);
            params.push((pointer, name));
        }
        let mut retptrs = Vec::new();
        for (i, ty) in ret.retptrs.iter().enumerate() {
            if i > 0 || func.params.len() > 0 {
                self.src.h(", ");
            }
            self.print_ty(iface, ty);
            self.src.h(" *");
            let name = format!("ret{}", i);
            self.src.h(&name);
            retptrs.push(name);
        }
        if func.params.len() == 0 && ret.retptrs.len() == 0 {
            self.src.h("void");
        }
        self.src.h(")");

        let sig = self.src.h[start..].to_string();
        self.src.h(";\n");

        CSig {
            sig,
            name,
            params,
            ret,
            retptrs,
        }
    }

    fn is_arg_by_pointer(&self, iface: &Interface, ty: &Type) -> bool {
        match ty {
            Type::Id(id) => match &iface.types[*id].kind {
                TypeDefKind::Type(t) => self.is_arg_by_pointer(iface, t),
                TypeDefKind::Variant(_) => true,
                TypeDefKind::Union(_) => true,
                TypeDefKind::Option(_) => true,
                TypeDefKind::Result(_) => true,
                TypeDefKind::Enum(_) => false,
                TypeDefKind::Flags(_) => false,
                TypeDefKind::Tuple(_) | TypeDefKind::Record(_) | TypeDefKind::List(_) => true,
                TypeDefKind::Future(_) => todo!("is_arg_by_pointer for future"),
                TypeDefKind::Stream(_) => todo!("is_arg_by_pointer for stream"),
            },
            Type::String => true,
            _ => false,
        }
    }

    fn type_string(&mut self, iface: &Interface, ty: &Type) -> String {
        // Getting a type string happens during codegen, and by default means
        // that this is a private type that's being generated. This means we
        // want to keep track of new anonymous types that are *only* mentioned
        // in methods like this, so we can place those types in the C file
        // instead of the header interface file.
        let prev = mem::take(&mut self.src.h);
        let prev_public = mem::take(&mut self.public_anonymous_types);
        let prev_private = mem::take(&mut self.private_anonymous_types);

        // Print the type, which will collect into the fields that we replaced
        // above.
        self.print_ty(iface, ty);

        // Reset our public/private sets back to what they were beforehand.
        // Note that `print_ty` always adds to the public set, so we're
        // inverting the meaning here by interpreting those as new private
        // types.
        let new_private = mem::replace(&mut self.public_anonymous_types, prev_public);
        assert!(self.private_anonymous_types.is_empty());
        self.private_anonymous_types = prev_private;

        // For all new private types found while we printed this type, if the
        // type isn't already public then it's a new private type.
        for id in new_private {
            if !self.public_anonymous_types.contains(&id) {
                self.private_anonymous_types.insert(id);
            }
        }

        mem::replace(&mut self.src.h, prev).into()
    }

    fn print_ty(&mut self, iface: &Interface, ty: &Type) {
        match ty {
            Type::Bool => self.src.h("bool"),
            Type::Char => self.src.h("uint32_t"), // TODO: better type?
            Type::U8 => self.src.h("uint8_t"),
            Type::S8 => self.src.h("int8_t"),
            Type::U16 => self.src.h("uint16_t"),
            Type::S16 => self.src.h("int16_t"),
            Type::U32 => self.src.h("uint32_t"),
            Type::S32 => self.src.h("int32_t"),
            Type::U64 => self.src.h("uint64_t"),
            Type::S64 => self.src.h("int64_t"),
            Type::Float32 => self.src.h("float"),
            Type::Float64 => self.src.h("double"),
            Type::String => {
                self.print_namespace(iface);
                self.src.h("string_t");
                self.needs_string = true;
            }
            Type::Id(id) => {
                let ty = &iface.types[*id];
                match &ty.name {
                    Some(name) => {
                        self.print_namespace(iface);
                        self.src.h(&name.to_snake_case());
                        self.src.h("_t");
                    }
                    None => match &ty.kind {
                        TypeDefKind::Type(t) => self.print_ty(iface, t),
                        _ => {
                            self.public_anonymous_types.insert(*id);
                            self.private_anonymous_types.remove(id);
                            self.print_namespace(iface);
                            self.print_ty_name(iface, &Type::Id(*id));
                            self.src.h("_t");
                        }
                    },
                }
            }
        }
    }

    fn print_ty_name(&mut self, iface: &Interface, ty: &Type) {
        match ty {
            Type::Bool => self.src.h("bool"),
            Type::Char => self.src.h("char32"),
            Type::U8 => self.src.h("u8"),
            Type::S8 => self.src.h("s8"),
            Type::U16 => self.src.h("u16"),
            Type::S16 => self.src.h("s16"),
            Type::U32 => self.src.h("u32"),
            Type::S32 => self.src.h("s32"),
            Type::U64 => self.src.h("u64"),
            Type::S64 => self.src.h("s64"),
            Type::Float32 => self.src.h("float32"),
            Type::Float64 => self.src.h("float64"),
            Type::String => self.src.h("string"),
            Type::Id(id) => {
                let ty = &iface.types[*id];
                if let Some(name) = &ty.name {
                    return self.src.h(&name.to_snake_case());
                }
                match &ty.kind {
                    TypeDefKind::Type(t) => self.print_ty_name(iface, t),
                    TypeDefKind::Record(_)
                    | TypeDefKind::Flags(_)
                    | TypeDefKind::Enum(_)
                    | TypeDefKind::Variant(_)
                    | TypeDefKind::Union(_) => {
                        unimplemented!()
                    }
                    TypeDefKind::Tuple(t) => {
                        self.src.h("tuple");
                        self.src.h(&t.types.len().to_string());
                        for ty in t.types.iter() {
                            self.src.h("_");
                            self.print_ty_name(iface, ty);
                        }
                    }
                    TypeDefKind::Option(ty) => {
                        self.src.h("option_");
                        self.print_ty_name(iface, ty);
                    }
                    TypeDefKind::Result(r) => {
                        self.src.h("result_");
                        self.print_optional_ty_name(iface, r.ok.as_ref());
                        self.src.h("_");
                        self.print_optional_ty_name(iface, r.err.as_ref());
                    }
                    TypeDefKind::List(t) => {
                        self.src.h("list_");
                        self.print_ty_name(iface, t);
                    }
                    TypeDefKind::Future(t) => {
                        self.src.h("future_");
                        self.print_optional_ty_name(iface, t.as_ref());
                    }
                    TypeDefKind::Stream(s) => {
                        self.src.h("stream_");
                        self.print_optional_ty_name(iface, s.element.as_ref());
                        self.src.h("_");
                        self.print_optional_ty_name(iface, s.end.as_ref());
                    }
                }
            }
        }
    }

    fn print_optional_ty_name(&mut self, iface: &Interface, ty: Option<&Type>) {
        match ty {
            Some(ty) => self.print_ty_name(iface, ty),
            None => self.src.h("void"),
        }
    }

    fn print_anonymous_type(&mut self, iface: &Interface, ty: TypeId) {
        let prev = mem::take(&mut self.src.h);
        self.src.h("typedef ");
        let kind = &iface.types[ty].kind;
        match kind {
            TypeDefKind::Type(_)
            | TypeDefKind::Flags(_)
            | TypeDefKind::Record(_)
            | TypeDefKind::Enum(_)
            | TypeDefKind::Variant(_)
            | TypeDefKind::Union(_) => {
                unreachable!()
            }
            TypeDefKind::Tuple(t) => {
                self.src.h("struct {\n");
                for (i, t) in t.types.iter().enumerate() {
                    self.print_ty(iface, t);
                    uwriteln!(self.src.h, " f{i};");
                }
                self.src.h("}");
            }
            TypeDefKind::Option(t) => {
                self.src.h("struct {\n");
                self.src.h("bool is_some;\n");
                if !self.is_empty_type(iface, t) {
                    self.print_ty(iface, t);
                    self.src.h(" val;\n");
                }
                self.src.h("}");
            }
            TypeDefKind::Result(r) => {
                self.src.h("struct {
                    bool is_err;
                    union {
                ");
                if let Some(ok) = self.get_nonempty_type(iface, r.ok.as_ref()) {
                    self.print_ty(iface, ok);
                    self.src.h(" ok;\n");
                }
                if let Some(err) = self.get_nonempty_type(iface, r.err.as_ref()) {
                    self.print_ty(iface, err);
                    self.src.h(" err;\n");
                }
                self.src.h("} val;\n");
                self.src.h("}");
            }
            TypeDefKind::List(t) => {
                self.src.h("struct {\n");
                self.print_ty(iface, t);
                self.src.h(" *ptr;\n");
                self.src.h("size_t len;\n");
                self.src.h("}");
            }
            TypeDefKind::Future(_) => todo!("print_anonymous_type for future"),
            TypeDefKind::Stream(_) => todo!("print_anonymous_type for stream"),
        }
        self.src.h(" ");
        self.print_namespace(iface);
        self.print_ty_name(iface, &Type::Id(ty));
        self.src.h("_t;\n");
        self.types.insert(ty, mem::replace(&mut self.src.h, prev));
    }

    fn is_empty_type(&self, iface: &Interface, ty: &Type) -> bool {
        let id = match ty {
            Type::Id(id) => *id,
            _ => return false,
        };
        match &iface.types[id].kind {
            TypeDefKind::Type(t) => self.is_empty_type(iface, t),
            TypeDefKind::Record(r) => r.fields.is_empty(),
            TypeDefKind::Tuple(t) => t.types.is_empty(),
            _ => false,
        }
    }

    fn get_nonempty_type<'o>(&self, iface: &Interface, ty: Option<&'o Type>) -> Option<&'o Type> {
        match ty {
            Some(ty) => {
                if self.is_empty_type(iface, ty) {
                    None
                } else {
                    Some(ty)
                }
            }
            None => None,
        }
    }

    fn print_intrinsics(&mut self) {
        // Note that these intrinsics are declared as `weak` so they can be
        // overridden from some other symbol.
        self.src.c("
            __attribute__((weak, export_name(\"cabi_realloc\")))
            void *cabi_realloc(
                void *ptr,
                size_t orig_size,
                size_t org_align,
                size_t new_size
            ) {
                void *ret = realloc(ptr, new_size);
                if (!ret)
                    abort();
                return ret;
            }
        ");
    }

    fn print_namespace(&mut self, iface: &Interface) {
        self.src.h(&iface.name.to_snake_case());
        self.src.h("_");
    }

    fn print_dtor(&mut self, iface: &Interface, id: TypeId) {
        let ty = Type::Id(id);
        if !self.owns_anything(iface, &ty) {
            return;
        }
        let pos = self.src.h.len();
        self.src.h("void ");
        self.print_namespace(iface);
        self.print_ty_name(iface, &ty);
        self.src.h("_free(");
        self.print_namespace(iface);
        self.print_ty_name(iface, &ty);
        self.src.h("_t *ptr)");

        self.src.c(&self.src.h[pos..].to_string());
        self.src.h(";\n");
        self.src.c(" {\n");
        match &iface.types[id].kind {
            TypeDefKind::Type(t) => self.free(iface, t, "ptr"),

            TypeDefKind::Flags(_) => {}
            TypeDefKind::Enum(_) => {}

            TypeDefKind::Record(r) => {
                for field in r.fields.iter() {
                    if !self.owns_anything(iface, &field.ty) {
                        continue;
                    }
                    self.free(
                        iface,
                        &field.ty,
                        &format!("&ptr->{}", field.name.to_snake_case()),
                    );
                }
            }

            TypeDefKind::Tuple(t) => {
                for (i, ty) in t.types.iter().enumerate() {
                    if !self.owns_anything(iface, ty) {
                        continue;
                    }
                    self.free(iface, ty, &format!("&ptr->f{i}"));
                }
            }

            TypeDefKind::List(t) => {
                if self.owns_anything(iface, t) {
                    self.src.c("for (size_t i = 0; i < ptr->len; i++) {\n");
                    self.free(iface, t, "&ptr->ptr[i]");
                    self.src.c("}\n");
                }
                uwriteln!(self.src.c, "if (ptr->len > 0) {{");
                uwriteln!(self.src.c, "free(ptr->ptr);");
                uwriteln!(self.src.c, "}}");
            }

            TypeDefKind::Variant(v) => {
                self.src.c("switch ((int32_t) ptr->tag) {\n");
                for (i, case) in v.cases.iter().enumerate() {
                    if let Some(ty) = &case.ty {
                        if !self.owns_anything(iface, ty) {
                            continue;
                        }
                        uwriteln!(self.src.c, "case {}: {{", i);
                        let expr = format!("&ptr->val.{}", case.name.to_snake_case());
                        if let Some(ty) = &case.ty {
                            self.free(iface, ty, &expr);
                        }
                        self.src.c("break;\n");
                        self.src.c("}\n");
                    }
                }
                self.src.c("}\n");
            }

            TypeDefKind::Union(u) => {
                self.src.c("switch ((int32_t) ptr->tag) {\n");
                for (i, case) in u.cases.iter().enumerate() {
                    if !self.owns_anything(iface, &case.ty) {
                        continue;
                    }
                    uwriteln!(self.src.c, "case {i}: {{");
                    let expr = format!("&ptr->val.f{i}");
                    self.free(iface, &case.ty, &expr);
                    self.src.c("break;\n");
                    self.src.c("}\n");
                }
                self.src.c("}\n");
            }

            TypeDefKind::Option(t) => {
                self.src.c("if (ptr->is_some) {\n");
                self.free(iface, t, "&ptr->val");
                self.src.c("}\n");
            }

            TypeDefKind::Result(r) => {
                self.src.c("if (!ptr->is_err) {\n");
                if let Some(ok) = &r.ok {
                    if self.owns_anything(iface, ok) {
                        self.free(iface, ok, "&ptr->val.ok");
                    }
                }
                if let Some(err) = &r.err {
                    if self.owns_anything(iface, err) {
                        self.src.c("} else {\n");
                        self.free(iface, err, "&ptr->val.err");
                    }
                }
                self.src.c("}\n");
            }
            TypeDefKind::Future(_) => todo!("print_dtor for future"),
            TypeDefKind::Stream(_) => todo!("print_dtor for stream"),
        }
        self.src.c("}\n");
    }

    fn owns_anything(&self, iface: &Interface, ty: &Type) -> bool {
        let id = match ty {
            Type::Id(id) => *id,
            Type::String => return true,
            _ => return false,
        };
        match &iface.types[id].kind {
            TypeDefKind::Type(t) => self.owns_anything(iface, t),
            TypeDefKind::Record(r) => r.fields.iter().any(|t| self.owns_anything(iface, &t.ty)),
            TypeDefKind::Tuple(t) => t.types.iter().any(|t| self.owns_anything(iface, t)),
            TypeDefKind::Flags(_) => false,
            TypeDefKind::Enum(_) => false,
            TypeDefKind::List(_) => true,
            TypeDefKind::Variant(v) => v
                .cases
                .iter()
                .any(|c| self.optional_owns_anything(iface, c.ty.as_ref())),
            TypeDefKind::Union(v) => v
                .cases
                .iter()
                .any(|case| self.owns_anything(iface, &case.ty)),
            TypeDefKind::Option(t) => self.owns_anything(iface, t),
            TypeDefKind::Result(r) => {
                self.optional_owns_anything(iface, r.ok.as_ref())
                    || self.optional_owns_anything(iface, r.err.as_ref())
            }
            TypeDefKind::Future(_) => todo!("owns_anything for future"),
            TypeDefKind::Stream(_) => todo!("owns_anything for stream"),
        }
    }

    fn optional_owns_anything(&self, iface: &Interface, ty: Option<&Type>) -> bool {
        match ty {
            Some(ty) => self.owns_anything(iface, ty),
            None => false,
        }
    }

    fn free(&mut self, iface: &Interface, ty: &Type, expr: &str) {
        let prev = mem::take(&mut self.src.h);
        self.print_namespace(iface);
        self.print_ty_name(iface, ty);
        let name = mem::replace(&mut self.src.h, prev);

        self.src.c(&name);
        self.src.c("_free(");
        self.src.c(expr);
        self.src.c(");\n");
    }

    fn docs(&mut self, docs: &Docs) {
        let docs = match &docs.contents {
            Some(docs) => docs,
            None => return,
        };
        for line in docs.trim().lines() {
            self.src.h("// ");
            self.src.h(line);
            self.src.h("\n");
        }
    }
}

impl Return {
    fn return_single(&mut self, iface: &Interface, ty: &Type, orig_ty: &Type) {
        let id = match ty {
            Type::Id(id) => *id,
            Type::String => {
                self.retptrs.push(*orig_ty);
                return;
            }
            _ => {
                self.scalar = Some(Scalar::Type(*orig_ty));
                return;
            }
        };
        match &iface.types[id].kind {
            TypeDefKind::Type(t) => return self.return_single(iface, t, orig_ty),

            // Flags are returned as their bare values, and enums are scalars
            TypeDefKind::Flags(_) | TypeDefKind::Enum(_) => {
                self.scalar = Some(Scalar::Type(*orig_ty));
                return;
            }

            // Unpack optional returns where a boolean discriminant is
            // returned and then the actual type returned is returned
            // through a return pointer.
            TypeDefKind::Option(ty) => {
                self.scalar = Some(Scalar::OptionBool(*ty));
                self.retptrs.push(*ty);
                return;
            }

            // Unpack `result<T, E>` returns where `E` looks like an enum
            // so we can return that in the scalar return and have `T` get
            // returned through the normal returns.
            TypeDefKind::Result(r) => {
                if let Some(Type::Id(err)) = r.err {
                    if let TypeDefKind::Enum(enum_) = &iface.types[err].kind {
                        self.scalar = Some(Scalar::ResultEnum {
                            err,
                            max_err: enum_.cases.len(),
                        });
                        if let Some(ok) = r.ok {
                            self.retptrs.push(ok);
                        }
                        return;
                    }
                }

                // fall through to the return pointer case
            }

            // These types are always returned indirectly.
            TypeDefKind::Tuple(_)
            | TypeDefKind::Record(_)
            | TypeDefKind::List(_)
            | TypeDefKind::Variant(_)
            | TypeDefKind::Union(_) => {}

            TypeDefKind::Future(_) => todo!("return_single for future"),
            TypeDefKind::Stream(_) => todo!("return_single for stream"),
        }

        self.retptrs.push(*orig_ty);
    }
}

impl Generator for C {
    fn preprocess_one(&mut self, iface: &Interface, dir: Direction) {
        let variant = Self::abi_variant(dir);
        self.sizes.fill(iface);
        self.in_import = variant == AbiVariant::GuestImport;
    }

    fn type_record(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        record: &Record,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        for field in record.fields.iter() {
            self.print_ty(iface, &field.ty);
            self.src.h(" ");
            self.src.h(&field.name.to_snake_case());
            self.src.h(";\n");
        }
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_tuple(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        tuple: &Tuple,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        for (i, ty) in tuple.types.iter().enumerate() {
            self.print_ty(iface, ty);
            uwriteln!(self.src.h, " f{i};");
        }
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_flags(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        flags: &Flags,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef ");
        let repr = flags_repr(flags);
        self.src.h(int_repr(repr));
        self.src.h(" ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        for (i, flag) in flags.flags.iter().enumerate() {
            uwriteln!(
                self.src.h,
                "#define {}_{}_{} (1 << {})",
                iface.name.to_shouty_snake_case(),
                name.to_shouty_snake_case(),
                flag.name.to_shouty_snake_case(),
                i,
            );
        }

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_variant(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        variant: &Variant,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        self.src.h(int_repr(variant.tag()));
        self.src.h(" tag;\n");
        self.src.h("union {\n");
        for case in variant.cases.iter() {
            if let Some(ty) = self.get_nonempty_type(iface, case.ty.as_ref()) {
                self.print_ty(iface, ty);
                self.src.h(" ");
                self.src.h(&case.name.to_snake_case());
                self.src.h(";\n");
            }
        }
        self.src.h("} val;\n");
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");
        for (i, case) in variant.cases.iter().enumerate() {
            uwriteln!(
                self.src.h,
                "#define {}_{}_{} {}",
                iface.name.to_shouty_snake_case(),
                name.to_shouty_snake_case(),
                case.name.to_shouty_snake_case(),
                i,
            );
        }

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_union(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        union: &Union,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        self.src.h(int_repr(union.tag()));
        self.src.h(" tag;\n");
        self.src.h("union {\n");
        for (i, case) in union.cases.iter().enumerate() {
            self.print_ty(iface, &case.ty);
            uwriteln!(self.src.h, " f{i};");
        }
        self.src.h("} val;\n");
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_option(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        payload: &Type,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        self.src.h("bool is_some;\n");
        if !self.is_empty_type(iface, payload) {
            self.print_ty(iface, payload);
            self.src.h(" val;\n");
        }
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_result(
        &mut self,
        iface: &Interface,
        id: TypeId,
        name: &str,
        result: &Result_,
        docs: &Docs,
    ) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef struct {\n");
        self.src.h("bool is_err;\n");
        self.src.h("union {\n");
        if let Some(ok) = self.get_nonempty_type(iface, result.ok.as_ref()) {
            self.print_ty(iface, ok);
            self.src.h(" ok;\n");
        }
        if let Some(err) = self.get_nonempty_type(iface, result.err.as_ref()) {
            self.print_ty(iface, err);
            self.src.h(" err;\n");
        }
        self.src.h("} val;\n");
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_enum(&mut self, iface: &Interface, id: TypeId, name: &str, enum_: &Enum, docs: &Docs) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.names.insert(&name.to_snake_case()).unwrap();
        self.src.h("typedef ");
        self.src.h(int_repr(enum_.tag()));
        self.src.h(" ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");
        for (i, case) in enum_.cases.iter().enumerate() {
            uwriteln!(
                self.src.h,
                "#define {}_{}_{} {}",
                iface.name.to_shouty_snake_case(),
                name.to_shouty_snake_case(),
                case.name.to_shouty_snake_case(),
                i,
            );
        }

        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_alias(&mut self, iface: &Interface, id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.src.h("typedef ");
        self.print_ty(iface, ty);
        self.src.h(" ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");
        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_list(&mut self, iface: &Interface, id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        let prev = mem::take(&mut self.src.h);
        self.docs(docs);
        self.src.h("typedef struct {\n");
        self.print_ty(iface, ty);
        self.src.h(" *ptr;\n");
        self.src.h("size_t len;\n");
        self.src.h("} ");
        self.print_namespace(iface);
        self.src.h(&name.to_snake_case());
        self.src.h("_t;\n");
        self.types.insert(id, mem::replace(&mut self.src.h, prev));
    }

    fn type_builtin(&mut self, iface: &Interface, _id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        drop((iface, _id, name, ty, docs));
    }

    fn import(&mut self, iface: &Interface, func: &Function) {
        let prev = mem::take(&mut self.src);
        let sig = iface.wasm_signature(AbiVariant::GuestImport, func);

        // In the private C file, print a function declaration which is the
        // actual wasm import that we'll be calling, and this has the raw wasm
        // signature.
        uwriteln!(
            self.src.c,
            "__attribute__((import_module(\"{}\"), import_name(\"{}\")))",
            iface.name,
            func.name
        );
        let import_name = self.names.tmp(&format!(
            "__wasm_import_{}_{}",
            iface.name.to_snake_case(),
            func.name.to_snake_case()
        ));
        match sig.results.len() {
            0 => self.src.c("void"),
            1 => self.src.c(wasm_type(sig.results[0])),
            _ => unimplemented!("multi-value return not supported"),
        }
        self.src.c(" ");
        self.src.c(&import_name);
        self.src.c("(");
        for (i, param) in sig.params.iter().enumerate() {
            if i > 0 {
                self.src.c(", ");
            }
            self.src.c(wasm_type(*param));
        }
        if sig.params.len() == 0 {
            self.src.c("void");
        }
        self.src.c(");\n");

        // Print the public facing signature into the header, and since that's
        // what we are defining also print it into the C file.
        let c_sig = self.print_sig(iface, func);
        self.src.c(&c_sig.sig);
        self.src.c(" {\n");

        let mut f = FunctionBindgen::new(self, c_sig, &import_name);
        for (pointer, param) in f.sig.params.iter() {
            f.locals.insert(param).unwrap();

            if *pointer {
                f.params.push(format!("*{}", param));
            } else {
                f.params.push(param.clone());
            }
        }
        for ptr in f.sig.retptrs.iter() {
            f.locals.insert(ptr).unwrap();
        }
        iface.call(
            AbiVariant::GuestImport,
            LiftLower::LowerArgsLiftResults,
            func,
            &mut f,
        );

        let FunctionBindgen { src, .. } = f;

        self.src.c(&String::from(src));
        self.src.c("}\n");

        let src = mem::replace(&mut self.src, prev);
        self.funcs
            .entry(iface.name.to_string())
            .or_insert(Vec::new())
            .push(Func { src });
    }

    fn export(&mut self, iface: &Interface, func: &Function) {
        let prev = mem::take(&mut self.src);
        let sig = iface.wasm_signature(AbiVariant::GuestExport, func);

        // Print the actual header for this function into the header file, and
        // it's what we'll be calling.
        let c_sig = self.print_sig(iface, func);

        // Generate, in the C source file, the raw wasm signature that has the
        // canonical ABI.
        uwriteln!(
            self.src.c,
            "__attribute__((export_name(\"{}\")))",
            func.name
        );
        let import_name = self.names.tmp(&format!(
            "__wasm_export_{}_{}",
            iface.name.to_snake_case(),
            func.name.to_snake_case()
        ));

        // need to copy this before mutable borrow of self
        let direction = self.direction;

        let mut f = FunctionBindgen::new(self, c_sig, &import_name);
        match sig.results.len() {
            0 => f.gen.src.c("void"),
            1 => f.gen.src.c(wasm_type(sig.results[0])),
            _ => unimplemented!("multi-value return not supported"),
        }
        f.gen.src.c(" ");
        f.gen.src.c(&import_name);
        f.gen.src.c("(");
        for (i, param) in sig.params.iter().enumerate() {
            if i > 0 {
                f.gen.src.c(", ");
            }
            let name = f.locals.tmp("arg");
            uwrite!(f.gen.src.c, "{} {}", wasm_type(*param), name);
            f.params.push(name);
        }
        if sig.params.len() == 0 {
            f.gen.src.c("void");
        }
        f.gen.src.c(") {\n");

        // Force linking to the component type object if this function is live
        uwrite!(
            f.gen.src.c,
            "(void) {};",
            component_type_object::linking_symbol(iface, direction)
        );

        // Perform all lifting/lowering and append it to our src.
        iface.call(
            AbiVariant::GuestExport,
            LiftLower::LiftArgsLowerResults,
            func,
            &mut f,
        );
        let FunctionBindgen { src, .. } = f;
        self.src.c(&src);
        self.src.c("}\n");

        if iface.guest_export_needs_post_return(func) {
            uwriteln!(
                self.src.c,
                "__attribute__((export_name(\"cabi_post_{}\")))",
                func.name
            );
            uwrite!(self.src.c, "void {import_name}_post_return(");

            let mut params = Vec::new();
            let mut c_sig = CSig {
                name: String::from("INVALID"),
                sig: String::from("INVALID"),
                params: Vec::new(),
                ret: Return {
                    return_multiple: false,
                    scalar: None,
                    retptrs: Vec::new(),
                },
                retptrs: Vec::new(),
            };
            for (i, result) in sig.results.iter().enumerate() {
                let name = format!("arg{i}");
                uwrite!(self.src.c, "{} {name}", wasm_type(*result));
                c_sig.params.push((false, name.clone()));
                params.push(name);
            }
            self.src.c.push_str(") {\n");

            let mut f = FunctionBindgen::new(self, c_sig, &import_name);
            f.params = params;
            iface.post_return(func, &mut f);
            let FunctionBindgen { src, .. } = f;
            self.src.c(&src);
            self.src.c("}\n");
        }

        let src = mem::replace(&mut self.src, prev);
        self.funcs
            .entry(iface.name.to_string())
            .or_insert(Vec::new())
            .push(Func { src });
    }

    fn finish_one(&mut self, iface: &Interface, files: &mut Files) {
        uwrite!(
            self.src.h,
            "\
                #ifndef __BINDINGS_{0}_H
                #define __BINDINGS_{0}_H
                #ifdef __cplusplus
                extern \"C\"
                {{
                #endif

                #include <stdint.h>
                #include <stdbool.h>
            ",
            iface.name.to_shouty_snake_case(),
        );
        uwrite!(
            self.src.c,
            "\
                #include <stdlib.h>
                #include <{}.h>

                extern void {}(void);
            ",
            iface.name.to_kebab_case(),
            component_type_object::linking_symbol(iface, self.direction),
        );

        self.print_intrinsics();

        // Continuously generate anonymous types while we continue to find more
        //
        // First we take care of the public set of anonymous types. This will
        // iteratively print them and also remove any references from the
        // private set if we happen to also reference them.
        while !self.public_anonymous_types.is_empty() {
            for ty in mem::take(&mut self.public_anonymous_types) {
                self.print_anonymous_type(iface, ty);
            }
        }

        // Next we take care of private types. To do this we have basically the
        // same loop as above, after we switch the sets. We record, however,
        // all private types in a local set here to later determine if the type
        // needs to be in the C file or the H file.
        //
        // Note though that we don't re-print a type (and consider it private)
        // if we already printed it above as part of the public set.
        let mut private_types = HashSet::new();
        self.public_anonymous_types = mem::take(&mut self.private_anonymous_types);
        while !self.public_anonymous_types.is_empty() {
            for ty in mem::take(&mut self.public_anonymous_types) {
                if self.types.contains_key(&ty) {
                    continue;
                }
                private_types.insert(ty);
                self.print_anonymous_type(iface, ty);
            }
        }

        if self.needs_string {
            uwrite!(
                self.src.h,
                "
                    typedef struct {{
                        char *ptr;
                        size_t len;
                    }} {0}_string_t;

                    void {0}_string_set({0}_string_t *ret, const char *s);
                    void {0}_string_dup({0}_string_t *ret, const char *s);
                    void {0}_string_free({0}_string_t *ret);
                ",
                iface.name.to_snake_case(),
            );
            self.src.c("#include <string.h>\n");
            uwrite!(
                self.src.c,
                "
                    void {0}_string_set({0}_string_t *ret, const char *s) {{
                        ret->ptr = (char*) s;
                        ret->len = strlen(s);
                    }}

                    void {0}_string_dup({0}_string_t *ret, const char *s) {{
                        ret->len = strlen(s);
                        ret->ptr = cabi_realloc(NULL, 0, 1, ret->len);
                        memcpy(ret->ptr, s, ret->len);
                    }}

                    void {0}_string_free({0}_string_t *ret) {{
                        if (ret->len > 0) {{
                            free(ret->ptr);
                        }}
                        ret->ptr = NULL;
                        ret->len = 0;
                    }}
                ",
                iface.name.to_snake_case(),
            );
        }

        // Afterwards print all types. Note that this print must be in a
        // topological order, so we
        for id in iface.topological_types() {
            if let Some(ty) = self.types.get(&id) {
                if private_types.contains(&id) {
                    self.src.c(ty);
                } else {
                    self.src.h(ty);
                    self.print_dtor(iface, id);
                }
            }
        }

        // Declare a statically-allocated return area, if needed. We only do
        // this for export bindings, because import bindings allocate their
        // return-area on the stack.
        if !self.in_import && self.return_pointer_area_size > 0 {
            uwrite!(
                self.src.c,
                "
                    __attribute__((aligned({})))
                    static uint8_t RET_AREA[{}];
                ",
                self.return_pointer_area_align,
                self.return_pointer_area_size,
            );
        }

        for (_module, funcs) in mem::take(&mut self.funcs) {
            for func in funcs {
                self.src.h(&func.src.h);
                self.src.c(&func.src.c);
            }
        }

        self.src.h("\
        #ifdef __cplusplus
        }
        #endif
        ");
        self.src.h("#endif\n");

        files.push(
            &format!("{}.c", iface.name.to_kebab_case()),
            self.src.c.as_bytes(),
        );
        files.push(
            &format!("{}.h", iface.name.to_kebab_case()),
            self.src.h.as_bytes(),
        );
        files.push(
            &format!("{}_component_type.o", iface.name.to_kebab_case()),
            component_type_object::object(iface, self.direction)
                .unwrap()
                .as_slice(),
        );
    }
}

struct FunctionBindgen<'a> {
    gen: &'a mut C,
    locals: Ns,
    // tmp: usize,
    src: wit_bindgen_core::Source,
    sig: CSig,
    func_to_call: &'a str,
    block_storage: Vec<wit_bindgen_core::Source>,
    blocks: Vec<(String, Vec<String>)>,
    payloads: Vec<String>,
    params: Vec<String>,
    wasm_return: Option<String>,
}

impl<'a> FunctionBindgen<'a> {
    fn new(gen: &'a mut C, sig: CSig, func_to_call: &'a str) -> FunctionBindgen<'a> {
        FunctionBindgen {
            gen,
            sig,
            locals: Default::default(),
            src: Default::default(),
            func_to_call,
            block_storage: Vec::new(),
            blocks: Vec::new(),
            payloads: Vec::new(),
            params: Vec::new(),
            wasm_return: None,
        }
    }

    fn store_op(&mut self, op: &str, loc: &str) {
        self.src.push_str(loc);
        self.src.push_str(" = ");
        self.src.push_str(op);
        self.src.push_str(";\n");
    }

    fn load(&mut self, ty: &str, offset: i32, operands: &[String], results: &mut Vec<String>) {
        results.push(format!("*(({}*) ({} + {}))", ty, operands[0], offset));
    }

    fn load_ext(&mut self, ty: &str, offset: i32, operands: &[String], results: &mut Vec<String>) {
        self.load(ty, offset, operands, results);
        let result = results.pop().unwrap();
        results.push(format!("(int32_t) ({})", result));
    }

    fn store(&mut self, ty: &str, offset: i32, operands: &[String]) {
        uwriteln!(
            self.src,
            "*(({}*)({} + {})) = {};",
            ty,
            operands[1],
            offset,
            operands[0]
        );
    }

    fn store_in_retptrs(&mut self, operands: &[String]) {
        assert_eq!(operands.len(), self.sig.retptrs.len());
        for (op, ptr) in operands.iter().zip(self.sig.retptrs.clone()) {
            self.store_op(op, &format!("*{}", ptr));
        }
    }
}

impl Bindgen for FunctionBindgen<'_> {
    type Operand = String;

    fn sizes(&self) -> &SizeAlign {
        &self.gen.sizes
    }

    fn push_block(&mut self) {
        let prev = mem::take(&mut self.src);
        self.block_storage.push(prev);
    }

    fn finish_block(&mut self, operands: &mut Vec<String>) {
        let to_restore = self.block_storage.pop().unwrap();
        let src = mem::replace(&mut self.src, to_restore);
        self.blocks.push((src.into(), mem::take(operands)));
    }

    fn return_pointer(&mut self, _iface: &Interface, size: usize, align: usize) -> String {
        self.gen.return_pointer_area_size = self.gen.return_pointer_area_size.max(size);
        self.gen.return_pointer_area_align = self.gen.return_pointer_area_align.max(align);
        let ptr = self.locals.tmp("ptr");

        if self.gen.in_import {
            // Declare a stack-allocated return area. We only do this for
            // imports, because exports need their return area to be live until
            // the post-return call.
            uwrite!(
                self.src,
                "
                    __attribute__((aligned({})))
                    uint8_t ret_area[{}];
                ",
                align,
                size,
            );
            uwriteln!(self.src, "int32_t {} = (int32_t) &ret_area;", ptr);
        } else {
            // Declare a statically-allocated return area.
            uwriteln!(self.src, "int32_t {} = (int32_t) &RET_AREA;", ptr);
        }

        ptr
    }

    fn is_list_canonical(&self, iface: &Interface, ty: &Type) -> bool {
        iface.all_bits_valid(ty)
    }

    fn emit(
        &mut self,
        iface: &Interface,
        inst: &Instruction<'_>,
        operands: &mut Vec<String>,
        results: &mut Vec<String>,
    ) {
        match inst {
            Instruction::GetArg { nth } => results.push(self.params[*nth].clone()),
            Instruction::I32Const { val } => results.push(val.to_string()),
            Instruction::ConstZero { tys } => {
                for _ in tys.iter() {
                    results.push("0".to_string());
                }
            }

            // TODO: checked?
            Instruction::U8FromI32 => results.push(format!("(uint8_t) ({})", operands[0])),
            Instruction::S8FromI32 => results.push(format!("(int8_t) ({})", operands[0])),
            Instruction::U16FromI32 => results.push(format!("(uint16_t) ({})", operands[0])),
            Instruction::S16FromI32 => results.push(format!("(int16_t) ({})", operands[0])),
            Instruction::U32FromI32 => results.push(format!("(uint32_t) ({})", operands[0])),
            Instruction::S32FromI32 | Instruction::S64FromI64 => results.push(operands[0].clone()),
            Instruction::U64FromI64 => results.push(format!("(uint64_t) ({})", operands[0])),

            Instruction::I32FromU8
            | Instruction::I32FromS8
            | Instruction::I32FromU16
            | Instruction::I32FromS16
            | Instruction::I32FromU32 => {
                results.push(format!("(int32_t) ({})", operands[0]));
            }
            Instruction::I32FromS32 | Instruction::I64FromS64 => results.push(operands[0].clone()),
            Instruction::I64FromU64 => {
                results.push(format!("(int64_t) ({})", operands[0]));
            }

            // f32/f64 have the same representation in the import type and in C,
            // so no conversions necessary.
            Instruction::F32FromFloat32
            | Instruction::F64FromFloat64
            | Instruction::Float32FromF32
            | Instruction::Float64FromF64 => {
                results.push(operands[0].clone());
            }

            // TODO: checked
            Instruction::CharFromI32 => {
                results.push(format!("(uint32_t) ({})", operands[0]));
            }
            Instruction::I32FromChar => {
                results.push(format!("(int32_t) ({})", operands[0]));
            }

            Instruction::Bitcasts { casts } => {
                for (cast, op) in casts.iter().zip(operands) {
                    let op = op;
                    match cast {
                        Bitcast::I32ToF32 | Bitcast::I64ToF32 => {
                            results
                                .push(format!("((union {{ int32_t a; float b; }}){{ {} }}).b", op));
                        }
                        Bitcast::F32ToI32 | Bitcast::F32ToI64 => {
                            results
                                .push(format!("((union {{ float a; int32_t b; }}){{ {} }}).b", op));
                        }
                        Bitcast::I64ToF64 => {
                            results.push(format!(
                                "((union {{ int64_t a; double b; }}){{ {} }}).b",
                                op
                            ));
                        }
                        Bitcast::F64ToI64 => {
                            results.push(format!(
                                "((union {{ double a; int64_t b; }}){{ {} }}).b",
                                op
                            ));
                        }
                        Bitcast::I32ToI64 => {
                            results.push(format!("(int64_t) {}", op));
                        }
                        Bitcast::I64ToI32 => {
                            results.push(format!("(int32_t) {}", op));
                        }
                        Bitcast::None => results.push(op.to_string()),
                    }
                }
            }

            Instruction::BoolFromI32 | Instruction::I32FromBool => {
                results.push(operands[0].clone());
            }

            Instruction::RecordLower { record, .. } => {
                let op = &operands[0];
                for f in record.fields.iter() {
                    results.push(format!("({}).{}", op, f.name.to_snake_case()));
                }
            }
            Instruction::RecordLift { ty, .. } => {
                let name = self.gen.type_string(iface, &Type::Id(*ty));
                let mut result = format!("({}) {{\n", name);
                for op in operands {
                    uwriteln!(result, "{},", op);
                }
                result.push_str("}");
                results.push(result);
            }

            Instruction::TupleLower { tuple, .. } => {
                let op = &operands[0];
                for i in 0..tuple.types.len() {
                    results.push(format!("({}).f{}", op, i));
                }
            }
            Instruction::TupleLift { ty, .. } => {
                let name = self.gen.type_string(iface, &Type::Id(*ty));
                let mut result = format!("({}) {{\n", name);
                for op in operands {
                    uwriteln!(result, "{},", op);
                }
                result.push_str("}");
                results.push(result);
            }

            // TODO: checked
            Instruction::FlagsLower { flags, ty, .. } => match flags_repr(flags) {
                Int::U8 | Int::U16 | Int::U32 => {
                    results.push(operands.pop().unwrap());
                }
                Int::U64 => {
                    let name = self.gen.type_string(iface, &Type::Id(*ty));
                    let tmp = self.locals.tmp("flags");
                    uwriteln!(self.src, "{name} {tmp} = {};", operands[0]);
                    results.push(format!("{tmp} & 0xffffffff"));
                    results.push(format!("({tmp} >> 32) & 0xffffffff"));
                }
            },

            Instruction::FlagsLift { flags, ty, .. } => match flags_repr(flags) {
                Int::U8 | Int::U16 | Int::U32 => {
                    results.push(operands.pop().unwrap());
                }
                Int::U64 => {
                    let name = self.gen.type_string(iface, &Type::Id(*ty));
                    let op0 = &operands[0];
                    let op1 = &operands[1];
                    results.push(format!("(({name}) ({op0})) | ((({name}) ({op1})) << 32)"));
                }
            },

            Instruction::VariantPayloadName => {
                let name = self.locals.tmp("payload");
                results.push(format!("*{}", name));
                self.payloads.push(name);
            }

            Instruction::VariantLower {
                variant,
                results: result_types,
                ..
            } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();
                let payloads = self
                    .payloads
                    .drain(self.payloads.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();

                let mut variant_results = Vec::with_capacity(result_types.len());
                for ty in result_types.iter() {
                    let name = self.locals.tmp("variant");
                    results.push(name.clone());
                    self.src.push_str(wasm_type(*ty));
                    self.src.push_str(" ");
                    self.src.push_str(&name);
                    self.src.push_str(";\n");
                    variant_results.push(name);
                }

                let expr_to_match = format!("({}).tag", operands[0]);

                uwriteln!(self.src, "switch ((int32_t) {}) {{", expr_to_match);
                for (i, ((case, (block, block_results)), payload)) in
                    variant.cases.iter().zip(blocks).zip(payloads).enumerate()
                {
                    uwriteln!(self.src, "case {}: {{", i);
                    if let Some(ty) = self.gen.get_nonempty_type(iface, case.ty.as_ref()) {
                        let ty = self.gen.type_string(iface, ty);
                        uwrite!(
                            self.src,
                            "const {} *{} = &({}).val",
                            ty,
                            payload,
                            operands[0],
                        );
                        self.src.push_str(".");
                        self.src.push_str(&case.name.to_snake_case());
                        self.src.push_str(";\n");
                    }
                    self.src.push_str(&block);

                    for (name, result) in variant_results.iter().zip(&block_results) {
                        uwriteln!(self.src, "{} = {};", name, result);
                    }
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
            }

            Instruction::VariantLift { variant, ty, .. } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();

                let ty = self.gen.type_string(iface, &Type::Id(*ty));
                let result = self.locals.tmp("variant");
                uwriteln!(self.src, "{} {};", ty, result);
                uwriteln!(self.src, "{}.tag = {};", result, operands[0]);
                uwriteln!(self.src, "switch ((int32_t) {}.tag) {{", result);
                for (i, (case, (block, block_results))) in
                    variant.cases.iter().zip(blocks).enumerate()
                {
                    uwriteln!(self.src, "case {}: {{", i);
                    self.src.push_str(&block);
                    assert!(block_results.len() == (case.ty.is_some() as usize));

                    if let Some(_) = self.gen.get_nonempty_type(iface, case.ty.as_ref()) {
                        let mut dst = format!("{}.val", result);
                        dst.push_str(".");
                        dst.push_str(&case.name.to_snake_case());
                        self.store_op(&block_results[0], &dst);
                    }
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
                results.push(result);
            }

            Instruction::UnionLower {
                union,
                results: result_types,
                ..
            } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - union.cases.len()..)
                    .collect::<Vec<_>>();
                let payloads = self
                    .payloads
                    .drain(self.payloads.len() - union.cases.len()..)
                    .collect::<Vec<_>>();

                let mut union_results = Vec::with_capacity(result_types.len());
                for ty in result_types.iter() {
                    let name = self.locals.tmp("unionres");
                    results.push(name.clone());
                    let ty = wasm_type(*ty);
                    uwriteln!(self.src, "{ty} {name};");
                    union_results.push(name);
                }

                let op0 = &operands[0];
                uwriteln!(self.src, "switch (({op0}).tag) {{");
                for (i, ((case, (block, block_results)), payload)) in
                    union.cases.iter().zip(blocks).zip(payloads).enumerate()
                {
                    uwriteln!(self.src, "case {i}: {{");
                    if !self.gen.is_empty_type(iface, &case.ty) {
                        let ty = self.gen.type_string(iface, &case.ty);
                        uwriteln!(self.src, "const {ty} *{payload} = &({op0}).val.f{i};");
                    }
                    self.src.push_str(&block);

                    for (name, result) in union_results.iter().zip(&block_results) {
                        uwriteln!(self.src, "{name} = {result};");
                    }
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
            }

            Instruction::UnionLift { union, ty, .. } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - union.cases.len()..)
                    .collect::<Vec<_>>();

                let ty = self.gen.type_string(iface, &Type::Id(*ty));
                let result = self.locals.tmp("unionres");
                uwriteln!(self.src, "{} {};", ty, result);
                uwriteln!(self.src, "{}.tag = {};", result, operands[0]);
                uwriteln!(self.src, "switch ((int32_t) {}.tag) {{", result);
                for (i, (_case, (block, block_results))) in
                    union.cases.iter().zip(blocks).enumerate()
                {
                    uwriteln!(self.src, "case {i}: {{");
                    self.src.push_str(&block);

                    assert!(block_results.len() == 1);
                    let dst = format!("{result}.val.f{i}");
                    self.store_op(&block_results[0], &dst);
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
                results.push(result);
            }

            Instruction::OptionLower {
                results: result_types,
                payload,
                ..
            } => {
                let (mut some, some_results) = self.blocks.pop().unwrap();
                let (mut none, none_results) = self.blocks.pop().unwrap();
                let some_payload = self.payloads.pop().unwrap();
                let _none_payload = self.payloads.pop().unwrap();

                for (i, ty) in result_types.iter().enumerate() {
                    let name = self.locals.tmp("option");
                    results.push(name.clone());
                    self.src.push_str(wasm_type(*ty));
                    self.src.push_str(" ");
                    self.src.push_str(&name);
                    self.src.push_str(";\n");
                    let some_result = &some_results[i];
                    uwriteln!(some, "{name} = {some_result};");
                    let none_result = &none_results[i];
                    uwriteln!(none, "{name} = {none_result};");
                }

                let op0 = &operands[0];
                let ty = self.gen.type_string(iface, payload);
                let bind_some = if self.gen.is_empty_type(iface, payload) {
                    String::new()
                } else {
                    format!("const {ty} *{some_payload} = &({op0}).val;")
                };
                uwrite!(
                    self.src,
                    "
                    if (({op0}).is_some) {{
                        {bind_some}
                        {some}
                    }} else {{
                        {none}
                    }}
                    "
                );
            }

            Instruction::OptionLift { payload, ty, .. } => {
                let (some, some_results) = self.blocks.pop().unwrap();
                let (none, none_results) = self.blocks.pop().unwrap();
                assert!(none_results.len() == 0);
                assert!(some_results.len() == 1);
                let some_result = &some_results[0];

                let ty = self.gen.type_string(iface, &Type::Id(*ty));
                let result = self.locals.tmp("option");
                uwriteln!(self.src, "{ty} {result};");
                let op0 = &operands[0];
                let set_some = if self.gen.is_empty_type(iface, payload) {
                    String::new()
                } else {
                    format!("{result}.val = {some_result};")
                };
                uwrite!(
                    self.src,
                    "switch ({op0}) {{
                        case 0: {{
                            {result}.is_some = false;
                            {none}
                            break;
                        }}
                        case 1: {{
                            {result}.is_some = true;
                            {some}
                            {set_some}
                            break;
                        }}
                    }}"
                );
                results.push(result);
            }

            Instruction::ResultLower {
                results: result_types,
                result,
                ..
            } => {
                let (mut err, err_results) = self.blocks.pop().unwrap();
                let (mut ok, ok_results) = self.blocks.pop().unwrap();
                let err_payload = self.payloads.pop().unwrap();
                let ok_payload = self.payloads.pop().unwrap();

                for (i, ty) in result_types.iter().enumerate() {
                    let name = self.locals.tmp("result");
                    results.push(name.clone());
                    self.src.push_str(wasm_type(*ty));
                    self.src.push_str(" ");
                    self.src.push_str(&name);
                    self.src.push_str(";\n");
                    let ok_result = &ok_results[i];
                    uwriteln!(ok, "{name} = {ok_result};");
                    let err_result = &err_results[i];
                    uwriteln!(err, "{name} = {err_result};");
                }

                let op0 = &operands[0];
                let bind_ok =
                    if let Some(ok) = self.gen.get_nonempty_type(iface, result.ok.as_ref()) {
                        let ok_ty = self.gen.type_string(iface, ok);
                        format!("const {ok_ty} *{ok_payload} = &({op0}).val.ok;")
                    } else {
                        String::new()
                    };
                let bind_err =
                    if let Some(err) = self.gen.get_nonempty_type(iface, result.err.as_ref()) {
                        let err_ty = self.gen.type_string(iface, err);
                        format!("const {err_ty} *{err_payload} = &({op0}).val.err;")
                    } else {
                        String::new()
                    };
                uwrite!(
                    self.src,
                    "
                    if (({op0}).is_err) {{
                        {bind_err}
                        {err}
                    }} else {{
                        {bind_ok}
                        {ok}
                    }}
                    "
                );
            }

            Instruction::ResultLift { result, ty, .. } => {
                let (err, err_results) = self.blocks.pop().unwrap();
                assert!(err_results.len() == (result.err.is_some() as usize));
                let (ok, ok_results) = self.blocks.pop().unwrap();
                assert!(ok_results.len() == (result.ok.is_some() as usize));

                let result_tmp = self.locals.tmp("result");
                let set_ok = if let Some(_) = self.gen.get_nonempty_type(iface, result.ok.as_ref())
                {
                    let ok_result = &ok_results[0];
                    format!("{result_tmp}.val.ok = {ok_result};")
                } else {
                    String::new()
                };
                let set_err =
                    if let Some(_) = self.gen.get_nonempty_type(iface, result.err.as_ref()) {
                        let err_result = &err_results[0];
                        format!("{result_tmp}.val.err = {err_result};")
                    } else {
                        String::new()
                    };

                let ty = self.gen.type_string(iface, &Type::Id(*ty));
                uwriteln!(self.src, "{ty} {result_tmp};");
                let op0 = &operands[0];
                uwrite!(
                    self.src,
                    "switch ({op0}) {{
                        case 0: {{
                            {result_tmp}.is_err = false;
                            {ok}
                            {set_ok}
                            break;
                        }}
                        case 1: {{
                            {result_tmp}.is_err = true;
                            {err}
                            {set_err}
                            break;
                        }}
                    }}"
                );
                results.push(result_tmp);
            }

            Instruction::EnumLower { .. } => results.push(format!("(int32_t) {}", operands[0])),
            Instruction::EnumLift { .. } => results.push(operands.pop().unwrap()),

            Instruction::ListCanonLower { .. } | Instruction::StringLower { .. } => {
                results.push(format!("(int32_t) ({}).ptr", operands[0]));
                results.push(format!("(int32_t) ({}).len", operands[0]));
            }
            Instruction::ListCanonLift { element, ty, .. } => {
                let list_name = self.gen.type_string(iface, &Type::Id(*ty));
                let elem_name = self.gen.type_string(iface, element);
                results.push(format!(
                    "({}) {{ ({}*)({}), (size_t)({}) }}",
                    list_name, elem_name, operands[0], operands[1]
                ));
            }
            Instruction::StringLift { .. } => {
                let list_name = self.gen.type_string(iface, &Type::String);
                results.push(format!(
                    "({}) {{ (char*)({}), (size_t)({}) }}",
                    list_name, operands[0], operands[1]
                ));
            }

            Instruction::ListLower { .. } => {
                let _body = self.blocks.pop().unwrap();
                results.push(format!("(int32_t) ({}).ptr", operands[0]));
                results.push(format!("(int32_t) ({}).len", operands[0]));
            }

            Instruction::ListLift { element, ty, .. } => {
                let _body = self.blocks.pop().unwrap();
                let list_name = self.gen.type_string(iface, &Type::Id(*ty));
                let elem_name = self.gen.type_string(iface, element);
                results.push(format!(
                    "({}) {{ ({}*)({}), (size_t)({}) }}",
                    list_name, elem_name, operands[0], operands[1]
                ));
            }
            Instruction::IterElem { .. } => results.push("e".to_string()),
            Instruction::IterBasePointer => results.push("base".to_string()),

            Instruction::CallWasm { sig, .. } => {
                match sig.results.len() {
                    0 => {}
                    1 => {
                        self.src.push_str(wasm_type(sig.results[0]));
                        let ret = self.locals.tmp("ret");
                        self.wasm_return = Some(ret.clone());
                        uwrite!(self.src, " {} = ", ret);
                        results.push(ret);
                    }
                    _ => unimplemented!(),
                }
                self.src.push_str(self.func_to_call);
                self.src.push_str("(");
                for (i, op) in operands.iter().enumerate() {
                    if i > 0 {
                        self.src.push_str(", ");
                    }
                    self.src.push_str(op);
                }
                self.src.push_str(");\n");
            }

            Instruction::CallInterface { module: _, func } => {
                let mut args = String::new();
                for (i, (op, (byref, _))) in operands.iter().zip(&self.sig.params).enumerate() {
                    if i > 0 {
                        args.push_str(", ");
                    }
                    if *byref {
                        let name = self.locals.tmp("arg");
                        let ty = self.gen.type_string(iface, &func.params[i].1);
                        uwriteln!(self.src, "{} {} = {};", ty, name, op);
                        args.push_str("&");
                        args.push_str(&name);
                    } else {
                        args.push_str(op);
                    }
                }
                match &self.sig.ret.scalar {
                    None => {
                        let mut retptrs = Vec::new();
                        for ty in self.sig.ret.retptrs.iter() {
                            let name = self.locals.tmp("ret");
                            let ty = self.gen.type_string(iface, ty);
                            uwriteln!(self.src, "{} {};", ty, name);
                            if args.len() > 0 {
                                args.push_str(", ");
                            }
                            args.push_str("&");
                            args.push_str(&name);
                            retptrs.push(name);
                        }
                        uwriteln!(self.src, "{}({});", self.sig.name, args);
                        results.extend(retptrs);
                    }
                    Some(Scalar::Void) => {
                        uwriteln!(self.src, "{}({});", self.sig.name, args);
                    }
                    Some(Scalar::Type(_)) => {
                        let ret = self.locals.tmp("ret");
                        let ty = self
                            .gen
                            .type_string(iface, func.results.iter_types().next().unwrap());
                        uwriteln!(self.src, "{} {} = {}({});", ty, ret, self.sig.name, args);
                        results.push(ret);
                    }
                    Some(Scalar::OptionBool(ty)) => {
                        let ret = self.locals.tmp("ret");
                        let val = self.locals.tmp("val");
                        if args.len() > 0 {
                            args.push_str(", ");
                        }
                        args.push_str("&");
                        args.push_str(&val);
                        let payload_ty = self.gen.type_string(iface, ty);
                        uwriteln!(self.src, "{} {};", payload_ty, val);
                        uwriteln!(self.src, "bool {} = {}({});", ret, self.sig.name, args);
                        let option_ty = self
                            .gen
                            .type_string(iface, func.results.iter_types().next().unwrap());
                        let option_ret = self.locals.tmp("ret");
                        uwrite!(
                            self.src,
                            "
                                {ty} {ret};
                                {ret}.is_some = {tag};
                                {ret}.val = {val};
                            ",
                            ty = option_ty,
                            ret = option_ret,
                            tag = ret,
                            val = val,
                        );
                        results.push(option_ret);
                    }
                    Some(Scalar::ResultEnum { err, max_err }) => {
                        let ret = self.locals.tmp("ret");
                        let mut ok_names = Vec::new();
                        for ty in self.sig.ret.retptrs.iter() {
                            let val = self.locals.tmp("ok");
                            if args.len() > 0 {
                                args.push_str(", ");
                            }
                            args.push_str("&");
                            args.push_str(&val);
                            let ty = self.gen.type_string(iface, ty);
                            uwriteln!(self.src, "{} {};", ty, val);
                            ok_names.push(val);
                        }
                        let err_ty = self.gen.type_string(iface, &Type::Id(*err));
                        uwriteln!(
                            self.src,
                            "{} {} = {}({});",
                            err_ty,
                            ret,
                            self.sig.name,
                            args,
                        );
                        let result_ty = self
                            .gen
                            .type_string(iface, func.results.iter_types().next().unwrap());
                        let result_ret = self.locals.tmp("ret");
                        uwrite!(
                            self.src,
                            "
                                {ty} {ret};
                                if ({tag} <= {max}) {{
                                    {ret}.is_err = true;
                                    {ret}.val.err = {tag};
                                }} else {{
                                    {ret}.is_err = false;
                                    {set_ok}
                                }}
                            ",
                            ty = result_ty,
                            ret = result_ret,
                            tag = ret,
                            max = max_err,
                            set_ok = if self.sig.ret.retptrs.len() == 0 {
                                String::new()
                            } else {
                                let name = ok_names.pop().unwrap();
                                format!("{}.val.ok = {};", result_ret, name)
                            },
                        );
                        results.push(result_ret);
                    }
                }
            }
            Instruction::Return { .. } if self.gen.in_import => match self.sig.ret.scalar {
                None => self.store_in_retptrs(operands),
                Some(Scalar::Void) => {
                    assert!(operands.is_empty());
                }
                Some(Scalar::Type(_)) => {
                    assert_eq!(operands.len(), 1);
                    self.src.push_str("return ");
                    self.src.push_str(&operands[0]);
                    self.src.push_str(";\n");
                }
                Some(Scalar::OptionBool(_)) => {
                    assert_eq!(operands.len(), 1);
                    let variant = &operands[0];
                    self.store_in_retptrs(&[format!("{}.val", variant)]);
                    self.src.push_str("return ");
                    self.src.push_str(&variant);
                    self.src.push_str(".is_some;\n");
                }
                Some(Scalar::ResultEnum { .. }) => {
                    assert_eq!(operands.len(), 1);
                    let variant = &operands[0];
                    if self.sig.retptrs.len() > 0 {
                        self.store_in_retptrs(&[format!("{}.val.ok", variant)]);
                    }
                    uwriteln!(self.src, "return {}.is_err ? {0}.val.err : -1;", variant);
                }
            },
            Instruction::Return { amt, .. } => {
                assert!(*amt <= 1);
                if *amt == 1 {
                    uwriteln!(self.src, "return {};", operands[0]);
                }
            }

            Instruction::I32Load { offset } => self.load("int32_t", *offset, operands, results),
            Instruction::I64Load { offset } => self.load("int64_t", *offset, operands, results),
            Instruction::F32Load { offset } => self.load("float", *offset, operands, results),
            Instruction::F64Load { offset } => self.load("double", *offset, operands, results),
            Instruction::I32Store { offset } => self.store("int32_t", *offset, operands),
            Instruction::I64Store { offset } => self.store("int64_t", *offset, operands),
            Instruction::F32Store { offset } => self.store("float", *offset, operands),
            Instruction::F64Store { offset } => self.store("double", *offset, operands),
            Instruction::I32Store8 { offset } => self.store("int8_t", *offset, operands),
            Instruction::I32Store16 { offset } => self.store("int16_t", *offset, operands),

            Instruction::I32Load8U { offset } => {
                self.load_ext("uint8_t", *offset, operands, results)
            }
            Instruction::I32Load8S { offset } => {
                self.load_ext("int8_t", *offset, operands, results)
            }
            Instruction::I32Load16U { offset } => {
                self.load_ext("uint16_t", *offset, operands, results)
            }
            Instruction::I32Load16S { offset } => {
                self.load_ext("int16_t", *offset, operands, results)
            }

            Instruction::GuestDeallocate { .. } => {
                uwriteln!(self.src, "free((void*) ({}));", operands[0]);
            }
            Instruction::GuestDeallocateString => {
                uwriteln!(self.src, "if (({}) > 0) {{", operands[1]);
                uwriteln!(self.src, "free((void*) ({}));", operands[0]);
                uwriteln!(self.src, "}}");
            }
            Instruction::GuestDeallocateVariant { blocks } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - blocks..)
                    .collect::<Vec<_>>();

                uwriteln!(self.src, "switch ((int32_t) {}) {{", operands[0]);
                for (i, (block, results)) in blocks.into_iter().enumerate() {
                    assert!(results.is_empty());
                    uwriteln!(self.src, "case {}: {{", i);
                    self.src.push_str(&block);
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
            }
            Instruction::GuestDeallocateList { element } => {
                let (body, results) = self.blocks.pop().unwrap();
                assert!(results.is_empty());
                let ptr = self.locals.tmp("ptr");
                let len = self.locals.tmp("len");
                uwriteln!(self.src, "int32_t {ptr} = {};", operands[0]);
                uwriteln!(self.src, "int32_t {len} = {};", operands[1]);
                let i = self.locals.tmp("i");
                uwriteln!(self.src, "for (int32_t {i} = 0; {i} < {len}; {i}++) {{");
                let size = self.gen.sizes.size(element);
                uwriteln!(self.src, "int32_t base = {ptr} + {i} * {size};");
                uwriteln!(self.src, "(void) base;");
                uwrite!(self.src, "{body}");
                uwriteln!(self.src, "}}");
                uwriteln!(self.src, "if ({len} > 0) {{");
                uwriteln!(self.src, "free((void*) ({ptr}));");
                uwriteln!(self.src, "}}");
            }

            i => unimplemented!("{:?}", i),
        }
    }
}

#[derive(Default)]
struct Source {
    h: wit_bindgen_core::Source,
    c: wit_bindgen_core::Source,
}

impl Source {
    fn c(&mut self, s: &str) {
        self.c.push_str(s);
    }
    fn h(&mut self, s: &str) {
        self.h.push_str(s);
    }
}

fn wasm_type(ty: WasmType) -> &'static str {
    match ty {
        WasmType::I32 => "int32_t",
        WasmType::I64 => "int64_t",
        WasmType::F32 => "float",
        WasmType::F64 => "double",
    }
}

fn int_repr(ty: Int) -> &'static str {
    match ty {
        Int::U8 => "uint8_t",
        Int::U16 => "uint16_t",
        Int::U32 => "uint32_t",
        Int::U64 => "uint64_t",
    }
}

fn flags_repr(f: &Flags) -> Int {
    match f.repr() {
        FlagsRepr::U8 => Int::U8,
        FlagsRepr::U16 => Int::U16,
        FlagsRepr::U32(1) => Int::U32,
        FlagsRepr::U32(2) => Int::U64,
        repr => panic!("unimplemented flags {:?}", repr),
    }
}
