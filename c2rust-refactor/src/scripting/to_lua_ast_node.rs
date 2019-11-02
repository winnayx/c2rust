use std::cell::{Ref, RefCell, RefMut};
use std::fmt::Debug;
use std::mem::swap;
use std::ops::DerefMut;
use std::rc::Rc;
use std::sync::Arc;

use c2rust_ast_builder::mk;
use rustc::hir::def::Res;
use rustc::hir::HirId;
use rustc_target::spec::abi::Abi;
use syntax::ast::*;
use syntax::ptr::P;
use syntax::mut_visit::*;
use syntax::parse::token::{DelimToken, Nonterminal, Token};
use syntax::parse::token::{Lit as TokenLit, LitKind as TokenLitKind};
use syntax::source_map::{DUMMY_SP, Spanned, dummy_spanned};
use syntax::symbol::{Symbol, sym};
use syntax::tokenstream::{DelimSpan, TokenStream, TokenTree};
use syntax::ThinVec;
use syntax_pos::{Span, SyntaxContext};

use rlua::{Context, Error, Function, MetaMethod, Result, Scope, ToLua, UserData, UserDataMethods, Value};
use rlua::prelude::LuaString;

use crate::ast_manip::{util, visit_nodes, AstName, AstNode, WalkAst};
use crate::ast_manip::fn_edit::{FnLike, FnKind};
use super::DisplayLuaError;

/// Refactoring module
// @module Refactor

fn find_subexpr<'a, C>(expr: &'a mut P<Expr>, cmp: &C) -> Result<Option<&'a mut P<Expr>>>
where
    C: Fn(&P<Expr>) -> bool,
{
    if cmp(expr) {
        return Ok(Some(expr));
    }

    match &mut expr.kind {
        ExprKind::Box(expr)
        | ExprKind::Unary(_, expr)
        | ExprKind::Cast(expr, _)
        | ExprKind::Type(expr, _)
        | ExprKind::While(expr, _, _)
        | ExprKind::ForLoop(_, expr, _, _)
        | ExprKind::Match(expr, _)
        | ExprKind::Closure(_, _, _, _, expr, _)
        | ExprKind::Await(expr)
        | ExprKind::Field(expr, _)
        | ExprKind::AddrOf(_, expr)
        | ExprKind::Repeat(expr, _)
        | ExprKind::Paren(expr)
        | ExprKind::Try(expr) => {
            if let Some(e) = find_subexpr(expr, cmp)? {
                return Ok(Some(e));
            };
        },
        ExprKind::Array(exprs)
        | ExprKind::MethodCall(_, exprs)
        | ExprKind::Tup(exprs) => {
            for expr in exprs.iter_mut() {
                if let Some(e) = find_subexpr(expr, cmp)? {
                    return Ok(Some(e));
                };
            }
        },
        ExprKind::Call(expr, exprs) => {
            if let Some(e) = find_subexpr(expr, cmp)? {
                return Ok(Some(e));
            };

            for expr in exprs.iter_mut() {
                if let Some(e) = find_subexpr(expr, cmp)? {
                    return Ok(Some(e));
                };
            }
        },
        ExprKind::Binary(_, expr1, expr2)
        | ExprKind::Assign(expr1, expr2)
        | ExprKind::AssignOp(_, expr1, expr2)
        | ExprKind::Index(expr1, expr2) => {
            if let Some(e) = find_subexpr(expr1, cmp)? {
                return Ok(Some(e));
            };

            if let Some(e) = find_subexpr(expr2, cmp)? {
                return Ok(Some(e));
            };
        },
        ExprKind::If(expr, _, opt_expr) => {
            if let Some(e) = find_subexpr(expr, cmp)? {
                return Ok(Some(e));
            };

            if let Some(expr) = opt_expr {
                if let Some(e) = find_subexpr(expr, cmp)? {
                    return Ok(Some(e));
                };
            }
        },
        _ => (),
    };

    Ok(None)
}

pub(crate) trait ToLuaExt {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>>;
}

pub(crate) trait ToLuaAstNode {
    fn to_lua_ast_node<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>>;
}

pub(crate) trait ToLuaScoped {
    fn to_lua_scoped<'lua, 'scope>(self, lua: Context<'lua>, scope: &Scope<'lua, 'scope>) -> Result<Value<'lua>>;
}

// Marker trait for types that are safe to always pass to Lua as `LuaAstNode`s.
// For now, everything implements it except for `Vec<Stmt>`. This lets
// us send `Vec<Stmt>` values as Lua tables using `to_lua_ext` and
// as `LuaAstNode`s using `to_lua_ast_node`
pub(crate) trait LuaAstNodeSafe {}

impl<T> ToLuaExt for T
    where T: Sized + ToLuaAstNode,
          LuaAstNode<T>: 'static + UserData + Send + LuaAstNodeSafe,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua_ast_node(lua)
    }
}

impl<T> ToLuaExt for &T
    where T: Sized + ToLuaExt + Clone,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        (*self).clone().to_lua_ext(lua)
    }
}

impl<T> ToLuaExt for Rc<T>
    where T: Sized + ToLuaExt + Clone,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        (*self).clone().to_lua_ext(lua)
    }
}

impl<T> ToLuaExt for RefCell<T>
    where T: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.into_inner().to_lua_ext(lua)
    }
}

impl<T> ToLuaExt for Option<T>
    where T: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        if let Some(x) = self {
            x.to_lua_ext(lua)
        } else {
            Ok(Value::Nil)
        }
    }
}

impl<T> ToLuaExt for Vec<T>
    where T: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        let vec = self.into_iter()
            .map(|x| x.to_lua_ext(lua))
            .collect::<Result<Vec<_>>>()?;
        vec.to_lua(lua)
    }
}

impl<T> ToLuaExt for ThinVec<T>
    where T: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        let v: Vec<T> = self.into();
        v.to_lua_ext(lua)
    }
}

impl<A, B> ToLuaExt for (A, B)
    where A: Sized + ToLuaExt,
          B: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        let (a, b) = self;
        vec![a.to_lua_ext(lua)?, b.to_lua_ext(lua)?].to_lua(lua)
    }
}

impl<A, B, C> ToLuaExt for (A, B, C)
    where A: Sized + ToLuaExt,
          B: Sized + ToLuaExt,
          C: Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        let (a, b, c) = self;
        vec![a.to_lua_ext(lua)?, b.to_lua_ext(lua)?, c.to_lua_ext(lua)?].to_lua(lua)
    }
}

impl<A, B, C> ToLuaExt for P<(A, B, C)>
    where A: 'static + Sized + ToLuaExt,
          B: 'static + Sized + ToLuaExt,
          C: 'static + Sized + ToLuaExt,
{
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        let (a, b, c) = self.into_inner();
        vec![a.to_lua_ext(lua)?, b.to_lua_ext(lua)?, c.to_lua_ext(lua)?].to_lua(lua)
    }
}


// Manual `ToLuaExt` implementation for leaf types
impl ToLuaExt for bool {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua(lua)
    }
}

impl ToLuaExt for u8 {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua(lua)
    }
}

impl ToLuaExt for u16 {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua(lua)
    }
}

impl ToLuaExt for u128 {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua(lua)
    }
}

impl ToLuaExt for usize {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_lua(lua)
    }
}

impl ToLuaExt for char {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.to_string().to_lua(lua)
    }
}

impl ToLuaExt for NodeId {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.as_u32().to_lua(lua)
    }
}

impl ToLuaExt for Ident {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.as_str().to_lua(lua)
    }
}

impl ToLuaExt for Symbol {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.as_str().to_lua(lua)
    }
}

impl ToLuaExt for AttrId {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.0.to_lua(lua)
    }
}

impl ToLuaExt for Abi {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        self.name().to_lua(lua)
    }
}


// The other Lua traits
impl<T> ToLuaAstNode for T
    where T: Sized,
          LuaAstNode<T>: 'static + UserData + Send,
{
    fn to_lua_ast_node<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        lua.create_userdata(LuaAstNode::new(self))?.to_lua(lua)
    }
}

impl<T> ToLuaScoped for T
    where T: 'static + Sized,
          LuaAstNode<T>: UserData,
{
    fn to_lua_scoped<'lua, 'scope>(self, lua: Context<'lua>, scope: &Scope<'lua, 'scope>) -> Result<Value<'lua>> {
        scope.create_static_userdata(LuaAstNode::new(self)).and_then(|v| v.to_lua(lua))
    }
}

/// Holds a rustc AST node that can be passed back and forth to Lua as a scoped,
/// static userdata. Implement AddMoreMethods for LuaAstNode<T> to support an AST node
/// T.
#[derive(Clone)]
pub(crate) struct LuaAstNode<T> (Arc<RefCell<T>>);

impl<T> LuaAstNode<T> {
    pub fn new(item: T) -> Self {
        Self(Arc::new(RefCell::new(item)))
    }

    pub fn borrow(&self) -> Ref<T> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> RefMut<T> {
        self.0.borrow_mut()
    }

    pub fn map<F>(&self, f: F)
        where F: Fn(&mut T)
    {
        f(self.0.borrow_mut().deref_mut());
    }
}

impl<T> LuaAstNode<T>
    where T: Clone
{
    // TODO: make sure we aren't leaking LuaAstNodes into Lua, never to be freed
    pub fn into_inner(self) -> T {
        match Arc::try_unwrap(self.0) {
            Ok(cell) => cell.into_inner(),
            Err(arc) => arc.borrow().clone(),
        }
    }
}

impl<T> LuaAstNode<T>
    where T: WalkAst
{
    pub fn walk<V: MutVisitor>(&self, visitor: &mut V) {
        self.borrow_mut().walk(visitor);
    }
}

pub(crate) trait AddMoreMethods: UserData {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M);
}

impl<T: UserData> AddMoreMethods for T {
    default fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(_methods: &mut M) {}
}

include!(concat!(env!("OUT_DIR"), "/lua_ast_node_gen.inc.rs"));

unsafe impl<T> Send for LuaAstNode<Spanned<T>> {}
impl<T> LuaAstNodeSafe for LuaAstNode<Spanned<T>> {}
impl<T> UserData for LuaAstNode<Spanned<T>>
    where T: ToLuaExt + Clone + Debug,
{
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_node", |lua_ctx, this, ()| {
          Ok(this.borrow().node.clone().to_lua_ext(lua_ctx))
        });
        methods.add_method("get_span", |lua_ctx, this, ()| {
          Ok(this.borrow().span.clone().to_lua_ext(lua_ctx))
        });
        methods.add_meta_method(
          MetaMethod::ToString,
          |_lua_ctx, this, ()| Ok(format!("{:?}", this.borrow())),
        );
    }
}

/// Item AST node handle
// @type ItemAstNode
#[allow(unused_doc_comments)]
impl AddMoreMethods for LuaAstNode<P<Item>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ident", |_lua_ctx, this, ident: LuaString| {
            this.borrow_mut().ident = Ident::from_str(ident.to_str()?);
            Ok(())
        });

        methods.add_method("get_vis", |_lua_ctx, this, ()| {
            Ok(this.borrow().vis.ast_name())
        });

        /// Visit statements
        // @function visit_stmts
        // @tparam function(LuaAstNode) callback Function to call when visiting each statement
        methods.add_method("visit_stmts", |lua_ctx, this, callback: Function| {
            visit_nodes(&**this.borrow(), |node: &Stmt| {
                callback.call::<_, ()>(node.clone().to_lua_ext(lua_ctx))
                .unwrap_or_else(|e| panic!("Lua callback failed in visit_stmts: {}", DisplayLuaError(e)));
            });
            Ok(())
        });

        methods.add_method("visit_items", |lua_ctx, this, callback: Function| {
            visit_nodes(&**this.borrow(), |node: &Item| {
                callback.call::<_, ()>(P(node.clone()).to_lua_ext(lua_ctx))
                .unwrap_or_else(|e| panic!("Lua callback failed in visit_items: {}", DisplayLuaError(e)));
            });
            Ok(())
        });

        methods.add_method("visit_foreign_items", |lua_ctx, this, callback: Function| {
            visit_nodes(&**this.borrow(), |node: &ForeignItem| {
                callback.call::<_, ()>(node.clone().to_lua_ext(lua_ctx))
                .unwrap_or_else(|e| panic!("Lua callback failed in visit_foreign_items: {}", DisplayLuaError(e)));
            });
            Ok(())
        });

        methods.add_method("get_node", |lua_ctx, this, ()| {
            match this.borrow().kind.clone() {
                ItemKind::Use(e) => Ok(e.to_lua_ext(lua_ctx)),
                node => Err(Error::external(format!("Item node {:?} not implemented yet", node))),
            }
        });

        methods.add_method("get_fields", |_lua_ctx, this, ()| {
            if let ItemKind::Struct(variant_data, _) = &this.borrow().kind {
                return Ok(Some(variant_data
                    .fields()
                    .iter()
                    .map(|f| LuaAstNode::new(f.clone()))
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });

        methods.add_method("get_arg_ids", |_lua_ctx, this, ()| {
            if let ItemKind::Fn(decl, ..) = &this.borrow().kind {
                return Ok(Some(decl
                    .inputs
                    .iter()
                    .map(|a| a.id.as_u32())
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });

        methods.add_method("get_args", |_lua_ctx, this, ()| {
            if let ItemKind::Fn(decl, ..) = &this.borrow().kind {
                return Ok(Some(decl
                    .inputs
                    .iter()
                    .map(|a| LuaAstNode::new(a.clone()))
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });

        methods.add_method("set_args", |_lua_ctx, this, args: Vec<LuaAstNode<Param>>| {
            if let ItemKind::Fn(decl, ..) = &mut this.borrow_mut().kind {
                decl.inputs = args.iter().map(|a| a.borrow().clone()).collect();
            }

            Ok(())
        });

        methods.add_method("add_lifetime", |_lua_ctx, this, string: LuaString| {
            let lt_str = string.to_str()?;

            add_item_lifetime(&mut this.borrow_mut().kind, lt_str);

            Ok(())
        });

        methods.add_method("get_trait_ref", |_lua_ctx, this, ()| {
            if let ItemKind::Impl(_, _, _, _, opt_trait_ref, ..) = &this.borrow().kind {
                return Ok(opt_trait_ref.as_ref().map(|tr| LuaAstNode::new(tr.path.clone())));
            }

            Ok(None)
        });

        // TODO: This needs to be tested when the bitfields derive is present
        methods.add_method("remove_copy_derive", |_lua_ctx, this, ()| {
            let attrs = &mut this.borrow_mut().attrs;

            let opt_idx = attrs.iter().position(|a| a.check_name(Symbol::intern("rustc_copy_clone_marker")));

            if let Some(idx) = opt_idx {
                attrs.remove(idx);
            }

            attrs.push(mk().call_attr("derive", vec!["Clone"]).into_attrs().remove(0));

            Ok(())
        });
    }
}

/// ForeignItem AST node handle
// @type ForeignItemAstNode
impl AddMoreMethods for LuaAstNode<ForeignItem> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ident", |_lua_ctx, this, ident: LuaString| {
            this.borrow_mut().ident = Ident::from_str(ident.to_str()?);
            Ok(())
        });

        methods.add_method("get_args", |_lua_ctx, this, ()| {
            if let ForeignItemKind::Fn(decl, ..) = &this.borrow().kind {
                return Ok(Some(decl
                    .inputs
                    .iter()
                    .map(|a| LuaAstNode::new(a.clone()))
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });
    }
}

/// QSelf AST node handle
//
// @type QSelfAstNode
impl AddMoreMethods for LuaAstNode<QSelf> {}


/// Path AST node handle
// @type PathAstNode
impl AddMoreMethods for LuaAstNode<P<Path>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("has_generic_args", |_lua_ctx, this, ()| {
            Ok(this.borrow().segments.iter().any(|s| s.args.is_some()))
        });
        methods.add_method("get_segments", |lua_ctx, this, ()| {
            this.borrow()
                .segments
                .iter()
                .map(|s| s.ident.to_lua_ext(lua_ctx))
                .collect::<Result<Vec<_>>>()
        });
        methods.add_method("set_segments", |_lua_ctx, this, new_segments: Vec<LuaString>| {
            let has_generic_args = this.borrow().segments.iter().any(|s| s.args.is_some());
            if has_generic_args {
                Err(Error::external("One or more path segments have generic args, cannot set segments as strings"))
            } else {
                this.borrow_mut().segments = new_segments.into_iter().map(|new_seg| {
                    Ok(PathSegment::from_ident(Ident::from_str(new_seg.to_str()?)))
                }).collect::<Result<Vec<_>>>()?;
                Ok(())
            }
        });
        methods.add_method("map_segments", |lua_ctx, this, callback: Function| {
            let new_segments = lua_ctx.scope(|scope| {
                let segments = this.borrow().segments.iter().map(|s| scope.create_static_userdata(LuaAstNode::new(s.clone())).unwrap()).collect::<Vec<_>>().to_lua(lua_ctx);
                callback.call::<_, Vec<LuaAstNode<PathSegment>>>(segments)
            }).unwrap();
            this.borrow_mut().segments = new_segments.into_iter().map(|s| s.into_inner()).collect();
            Ok(())
        });

        methods.add_method("set_generic_angled_arg_tys", |_lua_ctx, this, (idx, tys): (usize, Vec<LuaAstNode<P<Ty>>>)| {
            let generic_arg = GenericArgs::AngleBracketed(AngleBracketedArgs {
                span: DUMMY_SP,
                args: tys.iter().map(|ty| GenericArg::Type(ty.borrow().clone())).collect(),
                constraints: Vec::new(),
            });

            // Using 1-offset idx like lua does
            this.borrow_mut().segments[idx - 1].args = Some(P(generic_arg));

            Ok(())
        });
    }
}

impl AddMoreMethods for LuaAstNode<PathSegment> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(_methods: &mut M) {
    }
}


/// Def result AST node handle
//
// This object is NOT thread-safe. Do not use an object of this class from a
// thread that did not acquire it.
// @type DefAstNode
unsafe impl Send for LuaAstNode<Res> {}
impl LuaAstNodeSafe for LuaAstNode<Res> {}
impl UserData for LuaAstNode<Res> {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_namespace", |_lua_ctx, this, ()| {
            Ok(util::namespace(&*this.borrow()).map(|namespace| namespace.descr()))
        });
    }
}

#[derive(Clone)]
struct SpanData(syntax_pos::SpanData);

impl UserData for SpanData {}

impl ToLuaExt for Span {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        lua.create_userdata(SpanData(self.data()))?.to_lua(lua)
    }
}


/// Expr AST node handle
// @type ExprAstNode
impl AddMoreMethods for LuaAstNode<P<Expr>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_node", |lua_ctx, this, ()| {
            match this.borrow().kind.clone() {
                ExprKind::Lit(x) => x.to_lua_ext(lua_ctx),
                node => Err(Error::external(format!("Expr node {:?} not implemented yet", node))),
            }
        });

        methods.add_method("get_ty", |lua_ctx, this, ()| {
            match &this.borrow().kind {
                ExprKind::Cast(_, ty)
                | ExprKind::Type(_, ty) => ty.to_lua_ext(lua_ctx),
                e => unimplemented!("LuaAstNode<P<Expr>>:get_ty() for {}", e.ast_name()),
            }
        });

        methods.add_method("set_ty", |_lua_ctx, this, lty: LuaAstNode<P<Ty>>| {
            match &mut this.borrow_mut().kind {
                ExprKind::Cast(_, ty)
                | ExprKind::Type(_, ty) => *ty = lty.borrow().clone(),
                e => unimplemented!("LuaAstNode<P<Expr>>:set_ty() for {}", e.ast_name()),
            }

            Ok(())
        });

        methods.add_method("get_ident", |lua_ctx, this, ()| {
            match &this.borrow().kind {
                ExprKind::Field(_, ident) => ident.to_lua_ext(lua_ctx).map(Some),
                _ => Ok(None),
            }
        });

        methods.add_method("get_path", |_lua_ctx, this, ()| {
            match &this.borrow().kind {
                ExprKind::Path(_, path)
                | ExprKind::Struct(path, ..) => Ok(Some(LuaAstNode::new(path.clone()))),
                _ => Ok(None),
            }
        });

        methods.add_method("get_exprs", |lua_ctx, this, ()| {
            match &this.borrow().kind {
                ExprKind::Cast(expr, _)
                | ExprKind::Field(expr, _)
                | ExprKind::Unary(_, expr) => vec![expr.clone()],
                ExprKind::MethodCall(_, exprs) => exprs.clone(),
                ExprKind::Assign(lhs, rhs)
                | ExprKind::AssignOp(_, lhs, rhs)
                | ExprKind::Binary(_, lhs, rhs) => vec![lhs.clone(), rhs.clone()],
                ExprKind::Call(func, params) => {
                    let mut exprs = Vec::with_capacity(params.len() + 1);

                    exprs.push(func.clone());

                    for param in params {
                        exprs.push(param.clone());
                    }

                    exprs
                },
                _ => Vec::new(),
            }.to_lua_ext(lua_ctx)
        });

        methods.add_method("set_exprs", |_lua_ctx, this, exprs: Vec<LuaAstNode<P<Expr>>>| {
            match &mut this.borrow_mut().kind {
                ExprKind::Field(expr, _)
                | ExprKind::Unary(_, expr) => *expr = exprs[0].borrow().clone(),
                ExprKind::MethodCall(_, exprs) => *exprs = exprs.to_vec(),
                ExprKind::Call(func, params) => {
                    *func = exprs[0].borrow().clone();
                    *params = exprs.iter().skip(1).map(|e| e.borrow().clone()).collect()
                },
                ExprKind::Assign(lhs, rhs)
                | ExprKind::AssignOp(_, lhs, rhs)
                | ExprKind::Binary(_, lhs, rhs) => {
                    *lhs = exprs[0].borrow().clone();
                    *rhs = exprs[1].borrow().clone();
                },
                ExprKind::Cast(expr, _) => *expr = exprs[0].borrow().clone(),
                ExprKind::AddrOf(_, expr) => *expr = exprs[0].borrow().clone(),
                e => unimplemented!("LuaAstNode<P<Expr>>:set_exprs() for {}", e.ast_name()),
            }

            Ok(())
        });

        methods.add_method("get_op", |_lua_ctx, this, ()| {
            match &this.borrow().kind {
                ExprKind::Unary(op, _) => Ok(Some(op.ast_name())),
                ExprKind::Binary(op, ..) |
                ExprKind::AssignOp(op, ..) => Ok(Some(op.ast_name())),
                _ => Ok(None),
            }
        });

        methods.add_method("get_method_name", |lua_ctx, this, ()| {
            if let ExprKind::MethodCall(path_seg, ..) = &this.borrow().kind {
                return path_seg.ident.as_str().to_lua(lua_ctx).map(Some)
            }

            Ok(None)
        });

        methods.add_method("set_method_name", |_lua_ctx, this, name: LuaString| {
            if let ExprKind::MethodCall(path_seg, ..) = &mut this.borrow_mut().kind {
                path_seg.ident = Ident::from_str(name.to_str()?);
            }

            Ok(())
        });

        methods.add_method("to_lit", |_lua_ctx, this, lit: LuaAstNode<Lit>| {
            let lit = lit.borrow().clone();

            this.borrow_mut().kind = ExprKind::Lit(lit);

            Ok(())
        });

        methods.add_method("to_index", |_lua_ctx, this, (indexed, indexee): (LuaAstNode<P<Expr>>, LuaAstNode<P<Expr>>)| {
            let indexed = indexed.borrow().clone();
            let indexee = indexee.borrow().clone();

            this.borrow_mut().kind = ExprKind::Index(indexed, indexee);

            Ok(())
        });

        type OptLuaExpr = Option<LuaAstNode<P<Expr>>>;

        methods.add_method("to_range", |_lua_ctx, this, (lhs, rhs): (OptLuaExpr, OptLuaExpr)| {
            let opt_lhs_expr = lhs.map(|e| e.borrow().clone());
            let opt_rhs_expr = rhs.map(|e| e.borrow().clone());

            this.borrow_mut().kind = ExprKind::Range(opt_lhs_expr, opt_rhs_expr, RangeLimits::HalfOpen);

            Ok(())
        });

        methods.add_method("to_binary", |_lua_ctx, this, (op, lhs, rhs): (LuaString, LuaAstNode<P<Expr>>, LuaAstNode<P<Expr>>)| {
            let op = match op.to_str()? {
                "Add" => BinOpKind::Add,
                "Div" => BinOpKind::Div,
                _ => unimplemented!("BinOpKind parsing from string"),
            };
            let lhs = lhs.borrow().clone();
            let rhs = rhs.borrow().clone();

            this.borrow_mut().kind = ExprKind::Binary(dummy_spanned(op), lhs, rhs);

            Ok(())
        });

        methods.add_method("to_unary", |_lua_ctx, this, (op, expr): (LuaString, LuaAstNode<P<Expr>>)| {
            let op = match op.to_str()? {
                "Deref" => UnOp::Deref,
                _ => unimplemented!("UnOp parsing from string"),
            };
            let expr = expr.borrow().clone();

            this.borrow_mut().kind = ExprKind::Unary(op, expr);

            Ok(())
        });

        methods.add_method("to_call", |_lua_ctx, this, exprs: Vec<LuaAstNode<P<Expr>>>| {
            let func = exprs[0].borrow().clone();
            let params = exprs.iter().skip(1).map(|lan| lan.borrow().clone()).collect();

            this.borrow_mut().kind = ExprKind::Call(func, params);

            Ok(())
        });

        methods.add_method("to_cast", |_lua_ctx, this, (expr, ty): (LuaAstNode<P<Expr>>, LuaAstNode<P<Ty>>)| {
            let expr = expr.borrow().clone();
            let ty = ty.borrow().clone();

            this.borrow_mut().kind = ExprKind::Cast(expr, ty);

            Ok(())
        });

        methods.add_method("to_addr_of", |_lua_ctx, this, (expr, mutable): (LuaAstNode<P<Expr>>, bool)| {
            let expr = expr.borrow().clone();
            let mutability = if mutable {
                Mutability::Mutable
            } else {
                Mutability::Immutable
            };

            this.borrow_mut().kind = ExprKind::AddrOf(mutability, expr);

            Ok(())
        });

        methods.add_method(
            "to_block",
            |_lua_ctx, this, (stmts, label, is_safe): (Vec<LuaAstNode<Stmt>>, Option<LuaString>, bool)|
        {
            let label = match label {
                Some(s) => Some(Label { ident: Ident::from_str(s.to_str()?) }),
                None => None,
            };
            let rules = if is_safe {
                BlockCheckMode::Default
            } else {
                BlockCheckMode::Unsafe(UnsafeSource::UserProvided)
            };
            let stmts = stmts.into_iter().map(|s| s.into_inner()).collect();
            let block = Block {
                stmts,
                id: DUMMY_NODE_ID,
                rules,
                span: DUMMY_SP,
            };

            this.borrow_mut().kind = ExprKind::Block(P(block), label);

            Ok(())
        });

        methods.add_method("to_stmt", |_lua_ctx, this, is_semi: bool| {
            let kind = if is_semi {
                StmtKind::Semi(this.borrow().clone())
            } else {
                StmtKind::Expr(this.borrow().clone())
            };

            Ok(LuaAstNode::new(Stmt {
                id: DUMMY_NODE_ID,
                kind,
                span: DUMMY_SP,
            }))
        });

        methods.add_method("to_method_call", |_lua_ctx, this, (segment, exprs): (LuaString, Vec<LuaAstNode<P<Expr>>>)| {
            let segment = PathSegment::from_ident(Ident::from_str(segment.to_str()?));
            let exprs = exprs.iter().map(|e| e.borrow().clone()).collect();

            this.borrow_mut().kind = ExprKind::MethodCall(segment, exprs);

            Ok(())
        });

        methods.add_method("to_closure", |_lua_ctx, this, (params, expr): (Vec<LuaString>, LuaAstNode<P<Expr>>)| {
            let expr = expr.borrow().clone();
            let inputs: Result<_> = params.into_iter().map(|s| Ok(Param {
                attrs: ThinVec::new(),
                ty: P(Ty {
                    id: DUMMY_NODE_ID,
                    kind: TyKind::Infer,
                    span: DUMMY_SP,
                }),
                pat: P(Pat {
                    id: DUMMY_NODE_ID,
                    kind: PatKind::Ident(
                        BindingMode::ByValue(Mutability::Immutable),
                        Ident::from_str(s.to_str()?),
                        None,
                    ),
                    span: DUMMY_SP,
                }),
                id: DUMMY_NODE_ID,
                span: DUMMY_SP,
                is_placeholder: true,
            })).collect();
            let fn_decl = P(FnDecl {
                inputs: inputs?,
                output: FunctionRetTy::Default(DUMMY_SP),
            });

            this.borrow_mut().kind = ExprKind::Closure(
                CaptureBy::Ref,
                IsAsync::NotAsync,
                Movability::Movable,
                fn_decl,
                expr,
                DUMMY_SP,
            );

            Ok(())
        });

        methods.add_method("to_field", |_lua_ctx, this, (expr, ident): (LuaAstNode<P<Expr>>, LuaString)| {
            let expr = expr.borrow().clone();

            this.borrow_mut().kind = ExprKind::Field(expr, Ident::from_str(ident.to_str()?));

            Ok(())
        });

        methods.add_method("to_ident_path", |_lua_ctx, this, path: LuaString| {
            let path = Path::from_ident(Ident::from_str(path.to_str()?));

            this.borrow_mut().kind = ExprKind::Path(None, path);

            Ok(())
        });

        methods.add_method("to_path", |_lua_ctx, this, path: LuaAstNode<Path>| {
            this.borrow_mut().kind = ExprKind::Path(None, path.borrow().clone());

            Ok(())
        });

        methods.add_method("to_bool_lit", |_lua_ctx, this, b: bool| {
            let lit = Lit {
                token: TokenLit {
                    kind: TokenLitKind::Bool,
                    symbol: Symbol::intern(&format!("{}", b)),
                    suffix: None,
                },
                kind: LitKind::Bool(b),
                span: DUMMY_SP,
            };

            this.borrow_mut().kind = ExprKind::Lit(lit);

            Ok(())
        });

        methods.add_method("find_subexpr", |lua_ctx, this, id: u32| {
            let expr = &mut *this.borrow_mut();
            let node_id = NodeId::from_u32(id);
            let opt_expr = find_subexpr(expr, &|sub_expr| sub_expr.id == node_id)?;

            opt_expr.cloned().to_lua_ext(lua_ctx)
        });

        methods.add_method("filtermap_subexprs", |_lua_ctx, this, (filter, map): (Function, Function)| {
            struct LuaFilterMapExpr<'lua> {
                filter: Function<'lua>,
                map: Function<'lua>,
            }

            impl<'lua> MutVisitor for LuaFilterMapExpr<'lua> {
                fn visit_expr(&mut self, x: &mut P<Expr>) {
                    let is_end = self.filter
                        .call::<_, bool>(x.kind.ast_name())
                        .expect("Failed to call filter");

                    if is_end {
                        *x = self.map
                            .call::<_, LuaAstNode<P<Expr>>>(LuaAstNode::new(x.clone()))
                            .expect("Failed to call map")
                            .into_inner();
                    } else {
                        noop_visit_expr(x, self);
                    }
                }
            }

            let mut visitor = LuaFilterMapExpr {
                filter,
                map,
            };

            visitor.visit_expr(&mut this.borrow_mut());

            Ok(())
        });
    }
}


/// Ty AST node handle
// @type TyAstNode
impl AddMoreMethods for LuaAstNode<P<Ty>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_tys", |lua_ctx, this, ()| {
            match &this.borrow().kind {
                TyKind::Slice(ty)
                | TyKind::Array(ty, _) => {
                    vec![ty.clone()].to_lua_ext(lua_ctx)
                },
                | TyKind::Tup(tys) => tys.clone().to_lua_ext(lua_ctx),
                e => unimplemented!("LuaAstNode<P<Ty>>:get_tys() for {}", e.ast_name()),
            }
        });

        methods.add_method("set_tys", |_lua_ctx, this, ltys: Vec<LuaAstNode<P<Ty>>>| {
            match &mut this.borrow_mut().kind {
                TyKind::Slice(ty)
                | TyKind::Array(ty, _) => *ty = ltys[0].borrow().clone(),
                | TyKind::Tup(tys) => {
                    tys.truncate(0);

                    for lty in ltys.iter() {
                        tys.push(lty.borrow().clone());
                    }
                },
                e => unimplemented!("LuaAstNode<P<Ty>>:set_tys() for {}", e.ast_name()),
            }

            Ok(())
        });

        methods.add_method("get_mut_ty", |_lua_ctx, this, ()| {
            match &this.borrow().kind {
                TyKind::Ptr(mut_ty) |
                TyKind::Rptr(_, mut_ty) => Ok(Some(LuaAstNode::new(mut_ty.clone()))),
                _ => Ok(None),
            }
        });

        methods.add_method("set_mut_ty", |_lua_ctx, this, mt: LuaAstNode<MutTy>| {
            match &mut this.borrow_mut().kind {
                TyKind::Ptr(mut_ty) |
                TyKind::Rptr(_, mut_ty) => {
                    *mut_ty = mt.borrow().clone();

                    Ok(())
                },
                _ => Ok(()),
            }
        });

        methods.add_method("get_path", |_lua_ctx, this, ()| {
            let path = match &this.borrow().kind {
                TyKind::Path(_, path) => path.clone(),
                _ => return Ok(None),
            };

            Ok(Some(LuaAstNode::new(path)))
        });

        methods.add_method("to_simple_path", |_lua_ctx, this, path: LuaString| {
            let path = Path::from_ident(Ident::from_str(path.to_str()?));

            this.borrow_mut().kind = TyKind::Path(None, path);

            Ok(())
        });

        methods.add_method("to_rptr", |_lua_ctx, this, (lt, mut_ty): (Option<LuaString>, LuaAstNode<MutTy>)| {
            let lt = lt.map(|lt| {
                let lt_str = lt.to_str()?;
                let mut lt_string = String::with_capacity(lt_str.len() + 1);

                lt_string.push('\'');
                lt_string.push_str(lt_str);

                Ok(Lifetime {
                    id: DUMMY_NODE_ID,
                    ident: Ident::from_str(&lt_string),
                })
            }).transpose()?;

            this.borrow_mut().kind = TyKind::Rptr(lt, mut_ty.borrow().clone());

            Ok(())
        });

        methods.add_method("wrap_in_slice", |_lua_ctx, this, ()| {
            let mut ty = this.borrow_mut();
            let mut placeholder = TyKind::Err;

            swap(&mut placeholder, &mut ty.kind);

            ty.kind = TyKind::Slice(P(Ty {
                id: DUMMY_NODE_ID,
                kind: placeholder,
                span: DUMMY_SP,
            }));

            Ok(())
        });

        methods.add_method("wrap_as_generic_angle_arg", |_lua_ctx, this, name: LuaString| {
            let mut ty = this.borrow_mut();
            let mut placeholder = TyKind::Err;

            swap(&mut placeholder, &mut ty.kind);

            let arg = GenericArg::Type(P(Ty {
                id: DUMMY_NODE_ID,
                kind: placeholder,
                span: DUMMY_SP,
            }));
            let args = GenericArgs::AngleBracketed(AngleBracketedArgs {
                span: DUMMY_SP,
                args: vec![arg],
                constraints: Vec::new(),
            });
            let path_segment = PathSegment {
                ident: Ident::from_str(name.to_str()?),
                id: DUMMY_NODE_ID,
                args: Some(P(args)),
            };
            let path = Path {
                span: DUMMY_SP,
                segments: vec![path_segment],
            };

            ty.kind = TyKind::Path(None, path);

            Ok(())
        });

        methods.add_method("add_lifetime", |_lua_ctx, this, lifetime: LuaString| {
            if let TyKind::Path(_, path) = &mut this.borrow_mut().kind {
                let lt_str = lifetime.to_str()?;
                let mut lt_string = String::with_capacity(lt_str.len() + 1);

                lt_string.push('\'');
                lt_string.push_str(lt_str);

                let segments_len = path.segments.len();
                let mut segment = &mut path.segments[segments_len - 1];
                let arg = GenericArg::Lifetime(Lifetime {
                    id: DUMMY_NODE_ID,
                    ident: Ident::from_str(&lt_string),
                });

                if let Some(generic_args) = &mut segment.args {
                    if let GenericArgs::AngleBracketed(args) = &mut **generic_args {
                        args.args.push(arg);
                    }
                } else {
                    let args = GenericArgs::AngleBracketed(AngleBracketedArgs {
                        span: DUMMY_SP,
                        args: vec![arg],
                        constraints: Vec::new(),
                    });

                    segment.args = Some(P(args));
                }
            }

            Ok(())
        });

        methods.add_method("map_ptr_root", |_lua_ctx, this, func: Function| {
            let ty = &mut *this.borrow_mut();

            fn apply_callback(ty: &mut P<Ty>, callback: Function) -> Result<()> {
                match &mut ty.kind {
                    TyKind::Rptr(_, mut_ty)
                    | TyKind::Ptr(mut_ty) => return apply_callback(&mut mut_ty.ty, callback),
                    _ => {
                        let ty_clone = ty.clone();
                        let new_ty = callback.call::<_, LuaAstNode<P<Ty>>>(LuaAstNode::new(ty_clone))?;

                        *ty = new_ty.into_inner();
                    },
                }

                Ok(())
            }

            apply_callback(ty, func)
        });
    }
}

unsafe impl Send for LuaAstNode<Vec<Stmt>> {}
impl UserData for LuaAstNode<Vec<Stmt>> {}

/// MutTy AST node handle
// @type MutTyAstNode
impl AddMoreMethods for LuaAstNode<MutTy> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ty", |_lua_ctx, this, ty: LuaAstNode<P<Ty>>| {
            this.borrow_mut().ty = ty.borrow().clone();

            Ok(())
        });

        methods.add_method("set_mutable", |_lua_ctx, this, mutable: bool| {
            this.borrow_mut().mutbl = if mutable {
                Mutability::Mutable
            } else {
                Mutability::Immutable
            };

            Ok(())
        });
    }
}

/// Stmt AST node handle
// @type StmtAstNode
impl AddMoreMethods for LuaAstNode<Stmt> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_node", |lua_ctx, this, ()| {
            match this.borrow().kind.clone() {
                StmtKind::Expr(e) | StmtKind::Semi(e) => e.to_lua_ext(lua_ctx),
                StmtKind::Local(l) => l.to_lua_ext(lua_ctx),
                StmtKind::Item(i) => i.to_lua_ext(lua_ctx),
                StmtKind::Mac(_) => Err(Error::external(String::from("Mac stmts aren't implemented yet"))),
            }
        });

        methods.add_method("to_expr", |_lua_ctx, this, (expr, is_semi): (LuaAstNode<P<Expr>>, bool)| {
            let expr = expr.borrow().clone();

            this.borrow_mut().kind = if is_semi {
                StmtKind::Semi(expr)
            } else {
                StmtKind::Expr(expr)
            };

            Ok(())
        });
    }
}


/// Pat AST node handle
// @type PatAstNode
impl AddMoreMethods for LuaAstNode<P<Pat>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_ident", |lua_ctx, this, ()| {
            if let PatKind::Ident(_, ident, _) = this.borrow().kind {
                ident.to_lua_ext(lua_ctx).map(Some)
            } else {
                Ok(None)
            }
        });
    }
}


/// Crate AST node handle
// @type CrateAstNode
impl AddMoreMethods for LuaAstNode<Crate> {}


/// Local AST node handle
// @type LocalAstNode
impl AddMoreMethods for LuaAstNode<P<Local>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ty", |_lua_ctx, this, ty: Option<LuaAstNode<P<Ty>>>| {
            this.borrow_mut().ty = ty.map(|ty| ty.borrow().clone());

            Ok(())
        });

        methods.add_method("set_init", |_lua_ctx, this, init: Option<LuaAstNode<P<Expr>>>| {
            this.borrow_mut().init = init.map(|init| init.borrow().clone());

            Ok(())
        });

        methods.add_method("get_pat", |_lua_ctx, this, ()| {
            Ok(LuaAstNode::new(this.borrow().pat.clone()))
        });

        methods.add_method("get_pat_id", |lua_ctx, this, ()| {
            Ok(this.borrow().pat.id.to_lua_ext(lua_ctx))
        });

        methods.add_method("get_attrs", |_lua_ctx, this, ()| {
            Ok(this.borrow()
                .attrs
                .iter()
                .map(|attr| LuaAstNode::new(attr.clone()))
                .collect::<Vec<_>>())
        });

        methods.add_method("to_stmt", |_lua_ctx, this, ()| {
            let kind = StmtKind::Local(this.borrow().clone());

            Ok(LuaAstNode::new(Stmt {
                id: DUMMY_NODE_ID,
                span: DUMMY_SP,
                kind,
            }))
        });
    }
}


/// Lit AST node handle
// @type LitAstNode
impl AddMoreMethods for LuaAstNode<Lit> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_kind", |_lua_ctx, this, ()| {
            Ok(this.borrow().kind.ast_name())
        });

        methods.add_method("get_value", |lua_ctx, this, ()| {
            match this.borrow().kind {
                LitKind::Str(s, _) => {
                    s.to_string().to_lua(lua_ctx)
                }
                LitKind::Int(i, _suffix) => i.to_lua_ext(lua_ctx),
                LitKind::Bool(b) => b.to_lua_ext(lua_ctx),
                LitKind::Char(c) => c.to_lua_ext(lua_ctx),
                ref node => {
                    Err(Error::external(format!(
                        "{:?} is not yet implemented",
                        node
                    )))
                }
            }
        });

        methods.add_method("replace_suffix", |_lua_ctx, this, lua_suffix: LuaString| {
            let mut lit = this.borrow_mut();

            match lit.kind {
                LitKind::Int(int, _) => {
                    let suffix = lua_suffix.to_str()?;
                    match suffix {
                        "" => {
                            lit.kind = LitKind::Int(int, LitIntType::Unsuffixed);
                            lit.token.suffix = None;
                            return Ok(());
                        }
                        "f32" | "f64" => {
                            let (float_ty, suffix_sym) = if suffix == "f32" {
                                (FloatTy::F32, sym::f32)
                            } else {
                                (FloatTy::F64, sym::f64)
                            };

                            let int_sym = Symbol::intern(&int.to_string());
                            lit.kind = LitKind::Float(int_sym, float_ty);
                            lit.token = TokenLit {
                                kind: TokenLitKind::Float,
                                symbol: int_sym,
                                suffix: Some(suffix_sym),
                            };
                            return Ok(());
                        }
                        _ => {}
                    }

                    macro_rules! impl_suffix_int_match {
                        ($([$suffix:ident, $outer:path, $inner:path]),*) => {
                            match suffix {
                                $(stringify!($suffix) => {
                                    lit.kind = LitKind::Int(int, $outer($inner));
                                    lit.token.suffix = Some(sym::$suffix);
                                    return Ok(());
                                })*
                                _ => {}
                            }
                        }
                    }
                    impl_suffix_int_match!(
                        [usize, LitIntType::Unsigned, UintTy::Usize],
                        [u8,    LitIntType::Unsigned, UintTy::U8],
                        [u16,   LitIntType::Unsigned, UintTy::U16],
                        [u32,   LitIntType::Unsigned, UintTy::U32],
                        [u64,   LitIntType::Unsigned, UintTy::U64],
                        [u128,  LitIntType::Unsigned, UintTy::U128],
                        [isize, LitIntType::Signed,    IntTy::Isize],
                        [i8,    LitIntType::Signed,    IntTy::I8],
                        [i16,   LitIntType::Signed,    IntTy::I16],
                        [i32,   LitIntType::Signed,    IntTy::I32],
                        [i64,   LitIntType::Signed,    IntTy::I64],
                        [i128,  LitIntType::Signed,    IntTy::I128]
                    );

                    Err(Error::external(format!(
                        "{} literal suffix is not yet implemented",
                        suffix
                    )))
                }

                _ => Err(Error::external(format!(
                        "LuaAstNode<Lit>::replace_suffix() is not yet implemented for {}",
                        lit.ast_name())))
            }
        });

        methods.add_method("strip_suffix", |_lua_ctx, this, ()| {
            let mut lit = this.borrow_mut();

            if let LitKind::Int(_, ref mut suffix) = lit.kind {
                *suffix = LitIntType::Unsuffixed
            }

            lit.token.suffix = None;

            Ok(())
        });

        methods.add_method("print", |_lua_ctx, this, ()| {
            println!("{:?}", this.borrow());

            Ok(())
        });
    }
}


/// Mod AST node handle
// @type ModAstNode
impl AddMoreMethods for LuaAstNode<Mod> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("num_items", |_lua_ctx, this, ()| {
            Ok(this.borrow().items.len())
        });

        methods.add_method_mut("insert_item", |_lua_ctx, this, (index, item): (usize, LuaAstNode<P<Item>>)| {
            this.borrow_mut().items.insert(index, item.borrow().clone());
            Ok(())
        });

        methods.add_method_mut("drain_items", |lua_ctx, this, ()| {
            this.borrow_mut()
                .items
                .drain(..)
                .map(|item| item.to_lua_ext(lua_ctx))
                .collect::<Result<Vec<_>>>()
        });
    }
}


/// UseTree AST node handle
// @type UseTreeAstNode
impl AddMoreMethods for LuaAstNode<P<UseTree>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_rename", |_lua_ctx, this, ()| {
            match this.borrow().kind {
                UseTreeKind::Simple(Some(rename), _, _) => Ok(Some(rename.to_string())),
                _ => Ok(None),
            }
        });

        methods.add_method("get_nested", |lua_ctx, this, ()| {
            match &this.borrow().kind {
                UseTreeKind::Nested(trees) => Ok(Some(
                    trees.clone()
                        .into_iter()
                        .map(|(tree, id)| Ok(vec![P(tree).to_lua_ext(lua_ctx)?, id.to_lua_ext(lua_ctx)?]))
                        .collect::<Result<Vec<_>>>()?
                )),
                _ => Ok(None),
            }
        });
    }
}

impl ToLuaExt for AstNode {
    fn to_lua_ext<'lua>(self, lua: Context<'lua>) -> Result<Value<'lua>> {
        match self {
            AstNode::Crate(x) => x.to_lua_ext(lua),
            AstNode::Expr(x) => x.to_lua_ext(lua),
            AstNode::Pat(x) => x.to_lua_ext(lua),
            AstNode::Ty(x) => x.to_lua_ext(lua),
            AstNode::Stmts(x) => x.to_lua_ext(lua),
            AstNode::Stmt(x) => x.to_lua_ext(lua),
            AstNode::Item(x) => x.to_lua_ext(lua),
        }
    }
}

/// FnLike AST node handle
// @type FnLikeAstNode
impl UserData for LuaAstNode<FnLike> {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_kind", |_lua_ctx, this, ()| {
            Ok(this.borrow().kind.ast_name())
        });

        methods.add_method("get_id", |lua_ctx, this, ()| {
            this.borrow().id.to_lua_ext(lua_ctx)
        });

        methods.add_method("get_ident", |lua_ctx, this, ()| {
            this.borrow().ident.to_lua_ext(lua_ctx)
        });

        methods.add_method("has_block", |lua_ctx, this, ()| {
            this.borrow().block.is_some().to_lua_ext(lua_ctx)
        });
    }
}

impl ToLuaExt for FnKind {
    fn to_lua_ext<'lua>(self, ctx: Context<'lua>) -> Result<Value<'lua>> {
        match self {
            FnKind::Normal => "Normal",
            FnKind::ImplMethod => "ImplMethod",
            FnKind::TraitMethod => "TraitMethod",
            FnKind::Foreign => "Foreign",
        }.to_lua(ctx)
    }
}

/// FnDecl AST node handle
// @type FnDeclAstNode
impl AddMoreMethods for LuaAstNode<P<FnDecl>> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("get_args", |_lua_ctx, this, ()| {
            this.borrow().inputs.clone().to_lua_ext(_lua_ctx)
        });
    }
}

/// Param AST node handle
// @type ParamAstNode
impl AddMoreMethods for LuaAstNode<Param> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ty", |_lua_ctx, this, ty: LuaAstNode<P<Ty>>| {
            this.borrow_mut().ty = ty.borrow().clone();

            Ok(())
        });

        methods.add_method("get_pat_id", |lua_ctx, this, ()| {
            Ok(this.borrow().pat.id.to_lua_ext(lua_ctx))
        });

        methods.add_method("set_binding", |_lua_ctx, this, binding_str: LuaString| {
            if let PatKind::Ident(binding, ..) = &mut this.borrow_mut().pat.kind {
                *binding = match binding_str.to_str()? {
                    "ByRefMut" => BindingMode::ByRef(Mutability::Mutable),
                    "ByRefImmut" => BindingMode::ByRef(Mutability::Immutable),
                    "ByValMut" => BindingMode::ByValue(Mutability::Mutable),
                    "ByValImmut" => BindingMode::ByValue(Mutability::Immutable),
                    _ => panic!("Unknown binding kind"),
                };
            }

            Ok(())
        });

        //methods.add_method("get_attrs", |_lua_ctx, this, ()| {
        //    Ok(this
        //       .borrow()
        //       .attrs
        //       .iter()
        //       .map(|attr| LuaAstNode::new(attr.clone()))
        //       .collect::<Vec<_>>())
        //});
    }
}

/// FnHeader AST node handle
// @type FnHeaderAstNode
impl AddMoreMethods for LuaAstNode<FnHeader> {}

/// StructField AST node handle
// @type StructFieldAstNode
impl AddMoreMethods for LuaAstNode<StructField> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("set_ty", |_lua_ctx, this, ty: LuaAstNode<P<Ty>>| {
            this.borrow_mut().ty = ty.borrow().clone();

            Ok(())
        });

        methods.add_method("get_attrs", |_lua_ctx, this, ()| {
            Ok(this.borrow()
                .attrs
                .iter()
                .map(|attr| LuaAstNode::new(attr.clone()))
                .collect::<Vec<_>>())
        });
    }
}

/// ItemKind AST node handle
// @type ItemKindAstNode
impl AddMoreMethods for LuaAstNode<ItemKind> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("add_lifetime", |_lua_ctx, this, string: LuaString| {
            let lt_str = string.to_str()?;

            add_item_lifetime(&mut this.borrow_mut(), lt_str);

            Ok(())
        });

        methods.add_method("get_field_ids", |_lua_ctx, this, ()| {
            if let ItemKind::Struct(variant_data, _) = &*this.borrow() {
                return Ok(Some(variant_data
                    .fields()
                    .iter()
                    .map(|f| f.id.as_u32())
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });

        methods.add_method("get_arg_ids", |_lua_ctx, this, ()| {
            if let ItemKind::Fn(decl, ..) = &*this.borrow() {
                return Ok(Some(decl
                    .inputs
                    .iter()
                    .map(|a| a.id.as_u32())
                    .collect::<Vec<_>>()
                ));
            }

            Ok(None)
        });
    }
}

/// Attribute AST node handle
// @type FnHeaderAstNode
impl AddMoreMethods for LuaAstNode<Attribute> {
    fn add_more_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("ident", |lua_ctx, this, ()| {
            if let Some(ident) = this.borrow().ident() {
                Ok(Some(ident.to_lua_ext(lua_ctx)?))
            } else {
                Ok(None)
            }
        });
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct LuaHirId(pub HirId);

impl UserData for LuaHirId {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua_ctx, this, rhs: LuaHirId| {
            Ok(*this == rhs)
        });

        methods.add_meta_method(MetaMethod::ToString, |_lua_ctx, this, ()| {
            Ok(format!("HirId {{ owner: {}, local_id: {} }}", this.0.owner.as_u32(), this.0.local_id.as_u32()))
        });
    }
}

fn add_item_lifetime(item_kind: &mut ItemKind, lt_name: &str) {
    let mut lt_string = String::with_capacity(lt_name.len() + 1);

    lt_string.push('\'');
    lt_string.push_str(lt_name);

    let generic_param = GenericParam {
        id: DUMMY_NODE_ID,
        ident: Ident::from_str(&lt_string),
        attrs: Default::default(),
        bounds: Vec::new(),
        kind: GenericParamKind::Lifetime,
        is_placeholder: false,
    };

    if let ItemKind::Struct(_, generics)
            | ItemKind::Fn(_, _, generics, _) = item_kind {
        let diff = |p: &GenericParam| {
            if let GenericParamKind::Lifetime = p.kind {
                p.ident == generic_param.ident
            } else {
                false
            }
        };

        if generics.params.iter().any(diff) {
            return;
        }

        generics.params.push(generic_param);
    }
}
