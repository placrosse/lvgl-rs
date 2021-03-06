use clang::{Clang, Entity, EntityKind, Index, Linkage, Type};
use inflector::cases::pascalcase::to_pascal_case;
use lazy_static::lazy_static;
use proc_macro2::{Ident, TokenStream};
use quote::format_ident;
use quote::quote;
use regex::Regex;
use std::collections::HashMap;
use std::error::Error;

type CGResult<T> = Result<T, Box<dyn Error>>;

const LIB_PREFIX: &str = "lv_";

lazy_static! {
    static ref TYPE_MAPPINGS: HashMap<&'static str, &'static str> = [
        ("uint16_t", "u16"),
        ("int32_t", "i32"),
        ("uint8_t", "u8"),
        ("bool", "bool"),
        ("_Bool", "bool"),
        ("const char *", "&str"),
    ]
    .iter()
    .cloned()
    .collect();
}

#[derive(Debug, Copy, Clone)]
pub enum WrapperError {
    Skip,
}

pub type WrapperResult<T> = Result<T, WrapperError>;

pub trait Rusty {
    type Parent;

    fn code(&self, parent: &Self::Parent) -> WrapperResult<TokenStream>;
}

#[derive(Clone, Eq, PartialEq)]
pub struct LvWidget {
    name: String,
    methods: Vec<LvFunc>,
}

impl Rusty for LvWidget {
    type Parent = ();

    fn code(&self, _parent: &Self::Parent) -> WrapperResult<TokenStream> {
        // We don't generate for the generic Obj
        if self.name.eq("obj") {
            return Err(WrapperError::Skip);
        }

        let widget_name = format_ident!("{}", to_pascal_case(self.name.as_str()));
        let methods: Vec<TokenStream> = self.methods.iter().flat_map(|m| m.code(self)).collect();
        Ok(quote! {
            define_object!(#widget_name);

            impl #widget_name {
                #(#methods)*
            }
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct LvFunc {
    name: String,
    args: Vec<LvArg>,
    ret: Option<LvType>,
}

impl LvFunc {
    pub fn new(name: String, args: Vec<LvArg>, ret: Option<LvType>) -> Self {
        Self { name, args, ret }
    }

    pub fn is_method(&self) -> bool {
        if self.args.len() > 0 {
            let first_arg = &self.args[0];
            return first_arg.typ.typ.contains("lv_obj_t");
        }
        false
    }
}

impl Rusty for LvFunc {
    type Parent = LvWidget;

    fn code(&self, parent: &Self::Parent) -> WrapperResult<TokenStream> {
        let templ = format!("{}{}_", LIB_PREFIX, parent.name.as_str());
        let new_name = self.name.replace(templ.as_str(), "");
        let func_name = format_ident!("{}", new_name);
        let original_func_name = format_ident!("{}", self.name.as_str());

        // generate constructor
        if new_name.eq("create") {
            return Ok(quote! {
                pub fn new<C>(parent: &mut C) -> crate::LvResult<Self>
                where
                    C: crate::NativeObject,
                {
                    unsafe {
                        let ptr = lvgl_sys::#original_func_name(parent.raw()?.as_mut(), core::ptr::null_mut());
                        let raw = core::ptr::NonNull::new(ptr)?;
                        let core = <crate::Obj as crate::Widget>::from_raw(raw);
                        Ok(Self { core })
                    }
                }
            });
        }

        // We don't deal with methods that return types yet
        if self.ret.is_some() {
            return Err(WrapperError::Skip);
        }

        // Make sure all arguments can be generated, skip the first arg (self)!
        for arg in self.args.iter().skip(1) {
            arg.code(self)?;
        }

        let args_decl = self
            .args
            .iter()
            .enumerate()
            .fold(quote!(), |args, (i, arg)| {
                // if first arg is `const`, then it should be immutable
                let next_arg = if i == 0 {
                    if arg.get_type().is_const() {
                        quote!(&self)
                    } else {
                        quote!(&mut self)
                    }
                } else {
                    arg.code(self).unwrap()
                };
                if args.is_empty() {
                    quote! {
                        #next_arg
                    }
                } else {
                    quote! {
                        #args, #next_arg
                    }
                }
            });

        let args_processing = self
            .args
            .iter()
            .enumerate()
            .fold(quote!(), |args, (i, arg)| {
                // if first arg is `const`, then it should be immutable
                let next_arg = if i == 0 {
                    quote!()
                } else {
                    let var = arg.get_processing();
                    quote!(#var)
                };
                if args.is_empty() {
                    quote! {
                        #next_arg
                    }
                } else {
                    quote! {
                        #args
                        #next_arg
                    }
                }
            });

        let args_call = self
            .args
            .iter()
            .enumerate()
            .fold(quote!(), |args, (i, arg)| {
                // if first arg is `const`, then it should be immutable
                let next_arg = if i == 0 {
                    quote!(self.core.raw()?.as_mut())
                } else {
                    let var = arg.get_value_usage();
                    quote!(#var)
                };
                if args.is_empty() {
                    quote! {
                        #next_arg
                    }
                } else {
                    quote! {
                        #args, #next_arg
                    }
                }
            });

        // TODO: Handle methods that return types
        Ok(quote! {
            pub fn #func_name(#args_decl) -> crate::LvResult<()> {
                #args_processing
                unsafe {
                    lvgl_sys::#original_func_name(#args_call);
                }
                Ok(())
            }
        })
    }
}

impl From<Entity<'_>> for LvFunc {
    fn from(entity: Entity) -> Self {
        let result = entity.get_result_type().unwrap().get_display_name();
        let ret_type = match result.as_str() {
            "void" => None,
            _ => Some(LvType::new(result)),
        };
        Self::new(
            entity.get_name().unwrap(),
            entity
                .get_arguments()
                .unwrap()
                .iter()
                .map(|e| e.into())
                .collect::<Vec<LvArg>>(),
            ret_type,
        )
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct LvArg {
    name: String,
    typ: LvType,
}

impl LvArg {
    pub fn new(name: String, typ: LvType) -> Self {
        Self { name, typ }
    }

    pub fn get_name_ident(&self) -> Ident {
        // Filter Rust language keywords
        syn::parse_str::<syn::Ident>(self.name.as_str())
            .unwrap_or_else(|_| format_ident!("r#{}", self.name.as_str()))
    }

    pub fn get_processing(&self) -> TokenStream {
        let ident = self.get_name_ident();
        // TODO: A better way to handle this, instead of `is_sometype()`, is using the Rust
        //       type system itself.
        if self.typ.is_str() {
            quote! {
                let #ident = cstr_core::CString::new(#ident)?;
            }
        } else {
            // No need to pre-process this type of argument
            quote! {}
        }
    }

    pub fn get_value_usage(&self) -> TokenStream {
        let ident = self.get_name_ident();
        if self.typ.is_str() {
            quote! {
                #ident.as_ptr()
            }
        } else {
            quote! {
                #ident
            }
        }
    }

    pub fn get_type(&self) -> &LvType {
        &self.typ
    }
}

impl Rusty for LvArg {
    type Parent = LvFunc;

    fn code(&self, _parent: &Self::Parent) -> WrapperResult<TokenStream> {
        let name = self.get_name_ident();
        let typ = self.typ.code(self)?;
        Ok(quote! {
            #name: #typ
        })
    }
}

impl From<&Entity<'_>> for LvArg {
    fn from(entity: &Entity) -> Self {
        Self::new(
            entity.get_name().unwrap(),
            entity.get_type().unwrap().into(),
        )
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct LvType {
    typ: String,
}

impl LvType {
    pub fn new(typ: String) -> Self {
        Self { typ }
    }

    pub fn is_const(&self) -> bool {
        self.typ.starts_with("const ")
    }

    pub fn is_str(&self) -> bool {
        self.typ.ends_with("char *")
    }
}

impl Rusty for LvType {
    type Parent = LvArg;

    fn code(&self, _parent: &Self::Parent) -> WrapperResult<TokenStream> {
        match TYPE_MAPPINGS.get(self.typ.as_str()) {
            Some(name) => {
                let val = if self.is_str() {
                    quote!(&str)
                } else {
                    let ident = format_ident!("{}", name);
                    quote!(#ident)
                };
                Ok(quote! {
                    #val
                })
            }
            None => Err(WrapperError::Skip),
        }
    }
}

impl From<Type<'_>> for LvType {
    fn from(ty: Type) -> Self {
        Self::new(ty.get_display_name())
    }
}

pub struct CodeGen {
    functions: Vec<LvFunc>,
    widgets: Vec<LvWidget>,
}

impl CodeGen {
    pub fn new() -> CGResult<Self> {
        let functions = Self::load_function_definitions()?;
        let widgets = Self::extract_widgets(&functions)?;
        Ok(Self { functions, widgets })
    }

    pub fn get_widgets(&self) -> &Vec<LvWidget> {
        &self.widgets
    }

    fn extract_widgets(functions: &Vec<LvFunc>) -> CGResult<Vec<LvWidget>> {
        let widget_names = Self::get_widget_names(functions);

        let widgets = functions.iter().fold(HashMap::new(), |mut ws, f| {
            for widget_name in &widget_names {
                if f.name
                    .starts_with(format!("{}{}", LIB_PREFIX, widget_name).as_str())
                    && f.is_method()
                {
                    ws.entry(widget_name.clone())
                        .or_insert_with(|| LvWidget {
                            name: widget_name.clone(),
                            methods: Vec::new(),
                        })
                        .methods
                        .push(f.clone())
                }
            }
            ws
        });

        Ok(widgets.values().map(|v| v.clone()).collect())
    }

    fn get_widget_names(functions: &Vec<LvFunc>) -> Vec<String> {
        let reg = format!("^{}([^_]+)_create$", LIB_PREFIX);
        let create_func = Regex::new(reg.as_str()).unwrap();

        functions
            .iter()
            .filter(|e| create_func.is_match(e.name.as_str()) && e.args.len() == 2)
            .filter_map(|f| {
                Some(String::from(
                    create_func
                        .captures(f.name.as_str())
                        .unwrap()
                        .get(1)
                        .unwrap()
                        .as_str(),
                ))
            })
            .collect::<Vec<_>>()
    }

    pub fn load_function_definitions() -> CGResult<Vec<LvFunc>> {
        let clang = Clang::new()?;
        let index = Index::new(&clang, false, false);
        let tu = index
            .parser(concat!(env!("OUT_DIR"), "/lvgl_full.c"))
            .parse()?;
        let entities = tu
            .get_entity()
            .get_children()
            .into_iter()
            .filter(|e| e.get_kind() == EntityKind::FunctionDecl)
            .filter(|e| e.get_name().is_some())
            .filter(|e| e.get_linkage().unwrap() != Linkage::Internal)
            .map(|e| e.into())
            .filter(|e: &LvFunc| e.name.starts_with(LIB_PREFIX))
            .collect::<Vec<_>>();
        Ok(entities)
    }

    pub fn get_function_names(&self) -> CGResult<Vec<String>> {
        Ok(self.functions.iter().map(|f| f.name.clone()).collect())
    }
}

#[cfg(test)]
mod test {
    use crate::{CodeGen, LvArg, LvFunc, LvType, LvWidget, Rusty};
    use quote::quote;

    #[test]
    fn can_list_functions() {
        let lv = CodeGen::new().unwrap();
        let func = String::from("lv_obj_create");
        let func_names = lv.get_function_names().unwrap();
        assert!(func_names.contains(&func));
    }

    #[test]
    fn can_identify_widgets_from_function_names() {
        let funcs = vec![
            LvFunc::new(
                "lv_obj_create".to_string(),
                vec![
                    LvArg::new("parent".to_string(), LvType::new("abc".to_string())),
                    LvArg::new("copy_from".to_string(), LvType::new("bcf".to_string())),
                ],
                None,
            ),
            LvFunc::new(
                "lv_btn_create".to_string(),
                vec![
                    LvArg::new("parent".to_string(), LvType::new("abc".to_string())),
                    LvArg::new("copy_from".to_string(), LvType::new("bcf".to_string())),
                ],
                None,
            ),
            LvFunc::new(
                "lv_do_something".to_string(),
                vec![
                    LvArg::new("parent".to_string(), LvType::new("abc".to_string())),
                    LvArg::new("copy_from".to_string(), LvType::new("bcf".to_string())),
                ],
                None,
            ),
            LvFunc::new(
                "lv_invalid_create".to_string(),
                vec![LvArg::new(
                    "parent".to_string(),
                    LvType::new("abc".to_string()),
                )],
                None,
            ),
            LvFunc::new(
                "lv_cb_create".to_string(),
                vec![
                    LvArg::new("parent".to_string(), LvType::new("abc".to_string())),
                    LvArg::new("copy_from".to_string(), LvType::new("bcf".to_string())),
                ],
                None,
            ),
        ];

        let widget_names = CodeGen::get_widget_names(&funcs);

        assert_eq!(widget_names.len(), 3);
    }

    #[test]
    fn generate_method_wrapper() {
        // void lv_arc_set_bg_end_angle(lv_obj_t * arc, uint16_t end);
        let arc_set_bg_end_angle = LvFunc::new(
            "lv_arc_set_bg_end_angle".to_string(),
            vec![
                LvArg::new("arc".to_string(), LvType::new("lv_obj_t *".to_string())),
                LvArg::new("end".to_string(), LvType::new("uint16_t".to_string())),
            ],
            None,
        );
        let arc_widget = LvWidget {
            name: "arc".to_string(),
            methods: vec![],
        };

        let code = arc_set_bg_end_angle.code(&arc_widget).unwrap();
        let expected_code = quote! {
            pub fn set_bg_end_angle(&mut self, end: u16) -> crate::LvResult<()> {
                unsafe {
                    lvgl_sys::lv_arc_set_bg_end_angle(self.core.raw()?.as_mut(), end);
                }
                Ok(())
            }
        };

        assert_eq!(code.to_string(), expected_code.to_string());
    }

    #[test]
    fn generate_method_wrapper_for_str_types_as_argument() {
        // void lv_label_set_text(lv_obj_t * label, const char * text)
        let label_set_text = LvFunc::new(
            "lv_label_set_text".to_string(),
            vec![
                LvArg::new("label".to_string(), LvType::new("lv_obj_t *".to_string())),
                LvArg::new("text".to_string(), LvType::new("const char *".to_string())),
            ],
            None,
        );
        let parent_widget = LvWidget {
            name: "label".to_string(),
            methods: vec![],
        };

        let code = label_set_text.code(&parent_widget).unwrap();
        let expected_code = quote! {
            pub fn set_text(&mut self, text: &str) -> crate::LvResult<()> {
                let text = cstr_core::CString::new(text)?;
                unsafe {
                    lvgl_sys::lv_label_set_text(
                        self.core.raw()?.as_mut(),
                        text.as_ptr()
                    );
                }
                Ok(())
            }
        };

        assert_eq!(code.to_string(), expected_code.to_string());
    }

    #[test]
    fn generate_basic_widget_code() {
        let arc_widget = LvWidget {
            name: "arc".to_string(),
            methods: vec![],
        };

        let code = arc_widget.code(&()).unwrap();
        let expected_code = quote! {
            define_object!(Arc);

            impl Arc {

            }
        };

        assert_eq!(code.to_string(), expected_code.to_string());
    }

    #[test]
    fn generate_widget_with_constructor_code() {
        // lv_obj_t * lv_arc_create(lv_obj_t * par, const lv_obj_t * copy);
        let arc_create = LvFunc::new(
            "lv_arc_create".to_string(),
            vec![
                LvArg::new("par".to_string(), LvType::new("lv_obj_t *".to_string())),
                LvArg::new(
                    "copy".to_string(),
                    LvType::new("const lv_obj_t *".to_string()),
                ),
            ],
            Some(LvType::new("lv_obj_t *".to_string())),
        );

        let arc_widget = LvWidget {
            name: "arc".to_string(),
            methods: vec![arc_create],
        };

        let code = arc_widget.code(&()).unwrap();
        let expected_code = quote! {
            define_object!(Arc);

            impl Arc {
                pub fn new<C>(parent: &mut C) -> crate::LvResult<Self>
                where
                    C: crate::NativeObject,
                {
                    unsafe {
                        let ptr = lvgl_sys::lv_arc_create(parent.raw()?.as_mut(), core::ptr::null_mut());
                        let raw = core::ptr::NonNull::new(ptr)?;
                        let core = <crate::Obj as crate::Widget>::from_raw(raw);
                        Ok(Self { core })
                    }
                }
            }
        };

        assert_eq!(code.to_string(), expected_code.to_string());
    }
}
