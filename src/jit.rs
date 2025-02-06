use std::collections::HashMap;
use crate::ast::{Expression, FunctionCall, InfixOp, Number, Statement, UnaryOp};
use crate::Source;
use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, Linkage, Module};

pub struct JIT {
    builder_context: FunctionBuilderContext,
    ctx: codegen::Context,
    data_description: DataDescription,
    module: JITModule,
}

impl JIT {

    pub fn new() -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder.set("is_pic", "false").unwrap();
        let isa_builder = cranelift_native::builder().unwrap_or_else(|msg| {
            panic!("host machine is not supported: {}", msg);
        });
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .unwrap();
        let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let module = JITModule::new(builder);
        let builder_context = FunctionBuilderContext::new();
        let ctx = module.make_context();
        let data_description = DataDescription::new();
        Self {
            builder_context,
            ctx,
            data_description,
            module,
        }
    }

    pub fn compile(&mut self, source: &Source) -> Result<(), String> {
        self.compile_fn(&[], &source.block)?;
        Ok(())
    }

    fn compile_fn(&mut self, params: &[String], block: &[Statement]) -> Result<*const u8, String> {
        let int = self.module.target_config().pointer_type();
        
        for _ in params {
            self.ctx.func.signature.params.push(AbiParam::new(int));
        }
        
        let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.builder_context);
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);
        let mut translator = Translator {
            int,
            builder,
            locals: HashMap::new(),
            module: &mut self.module,
            string_counter: 0,
        };
        
        for (i, param) in params.iter().enumerate() {
            let val = translator.builder.block_params(entry_block)[i];
            let var = translator.declare_local(param);
            translator.builder.def_var(var, val);
        }
        
        for stmt in block {
            translator.translate_statement(stmt);
        }
    
        translator.builder.finalize();
        
        let id = self
            .module
            .declare_function("main", Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        
        self.module
            .define_function(id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
        
        self.module.clear_context(&mut self.ctx);
        
        
        
        self.module.finalize_definitions().unwrap();
        
        let code = self.module.get_finalized_function(id);
        Ok(code)
    }
}
impl Default for JIT {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
struct Translator<'a> {
    int: types::Type,
    builder: FunctionBuilder<'a>,
    locals: HashMap<String, Variable>,
    module: &'a mut JITModule,
    string_counter: usize,
}
impl Translator<'_> {
    fn translate_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::If {
                ref ifcases,
                ref elsecase,
            } => {
                let merge_block = self.builder.create_block();
                for (cond, block) in ifcases {
                    let cond_value = self.translate_expr(cond);
                    let then_block = self.builder.create_block();
                    let else_block = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(cond_value, then_block, &[], else_block, &[]);
                    
                    self.builder.switch_to_block(then_block);
                    self.builder.seal_block(then_block);
                    for stmt in block {
                        self.translate_statement(stmt);
                    }
                    self.builder.ins().jump(merge_block, &[]);
                    self.builder.switch_to_block(else_block);
                    self.builder.seal_block(else_block);
                }
                
                for stmt in elsecase {
                    self.translate_statement(stmt);
                }
                
                self.builder.ins().jump(merge_block, &[]);
                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);
            }
            Statement::While {
                ref cond,
                ref block,
            } => {
                let header_block = self.builder.create_block();
                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();
                self.builder.ins().jump(header_block, &[]);
                self.builder.switch_to_block(header_block); 
                let cond_value = self.translate_expr(cond);
                self.builder
                    .ins()
                    .brif(cond_value, body_block, &[], exit_block, &[]);
                
                self.builder.switch_to_block(body_block);
                self.builder.seal_block(body_block);
                for stmt in block {
                    self.translate_statement(stmt);
                }
                self.builder.ins().jump(header_block, &[]);
                self.builder.switch_to_block(exit_block);
                self.builder.seal_block(header_block); 
                self.builder.seal_block(exit_block);
            }
            Statement::Repeat {
                ref block,
                ref cond,
            } => {
                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();
                self.builder.ins().jump(body_block, &[]);
                self.builder.switch_to_block(body_block); 
                for stmt in block {
                    self.translate_statement(stmt);
                }
                let cond_value = self.translate_expr(cond);
                self.builder
                    .ins()
                    .brif(cond_value, body_block, &[], exit_block, &[]);
                self.builder.switch_to_block(exit_block);
                self.builder.seal_block(body_block); 
                self.builder.seal_block(exit_block);
            }
            Statement::Local {
                ref vars,
                ref exprs,
            } => {
                let values: Vec<_> = exprs.iter().map(|e| self.translate_expr(e)).collect();
                for ((name, _attr), val) in vars.iter().zip(values.into_iter()) {
                    let var = self.declare_local(name.as_str());
                    self.builder.def_var(var, val);
                }
            }
            Statement::Assign {
                ref vars,
                ref exprs,
            } => {
                let values: Vec<_> = exprs.iter().map(|e| self.translate_expr(e)).collect();
                for (var, val) in vars.iter().zip(values.into_iter()) {
                    match var {
                        Expression::Var(name) => {
                            if let Some(var) = self.locals.get(name) {
                                self.builder.def_var(*var, val);
                            } else {
                                let data_id = self.module
                                    .declare_data(
                                        name,
                                        Linkage::Export,
                                        true,
                                        false,
                                    )
                                    .expect("Failed to declare global variable data");
                                let mut data_desc = DataDescription::new();
                                data_desc.define_zeroinit(8);
                                let _ = self.module.define_data(data_id, &data_desc);
                                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                                let ptr = self.builder.ins().symbol_value(self.int, local_id);
                                self.builder.ins().store(MemFlags::trusted(), val, ptr, 0);
                            }
                        }
                        Expression::Field(table, field_name) => {
                            if let Expression::Var(table_name) = &**table {
                                if table_name == "_G" {
                                    let data_id = self.module
                                        .declare_data(
                                            field_name,
                                            Linkage::Export,
                                            true,  
                                            false, 
                                        )
                                        .expect("Failed to declare global variable data");
                                    let mut data_desc = DataDescription::new();
                                    data_desc.define_zeroinit(8); 
                                    let _ = self.module.define_data(data_id, &data_desc);
                                    let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                                    let ptr = self.builder.ins().symbol_value(self.int, local_id);
                                    self.builder.ins().store(MemFlags::trusted(), val, ptr, 0);
                                } else {
                                    todo!("not implemented assignment for global table: {:?}", table_name);
                                }
                            } else {
                                todo!("not implemented assignment for field with non-simple table: {:?}", table);
                            }
                        }
                        _ => todo!("not implemented assignment for {:?}", var),
                    }
                }
            }
            Statement::Return(e) => {
                let values: Vec<_> = e.iter().map(|e| self.translate_expr(e)).collect();
                self.builder.ins().return_(&values);
                let next_block = self.builder.create_block();
                self.builder.switch_to_block(next_block);
                self.builder.seal_block(next_block);
            }
            Statement::FunctCall(call) => {
                self.translate_function_call(call);
            }
            Statement::Do(block) => {
                for stmt in block {
                    self.translate_statement(stmt);
                }
            }
            Statement::Function { ref name, ref params, ref body } => {
                let func_name = name.qname.join(".");

                let mut new_ctx = self.module.make_context();
                for _ in &params.names {
                    new_ctx.func.signature.params.push(AbiParam::new(self.int));
                }
                new_ctx.func.signature.returns.push(AbiParam::new(self.int));

                let mut new_builder_context = FunctionBuilderContext::new();
                let mut builder = FunctionBuilder::new(&mut new_ctx.func, &mut new_builder_context);

                let entry_block = builder.create_block();
                builder.append_block_params_for_function_params(entry_block);
                builder.switch_to_block(entry_block);
                builder.seal_block(entry_block);

                let mut func_translator = Translator {
                    int: self.int,
                    builder,
                    locals: HashMap::new(),
                    module: self.module,
                    string_counter: self.string_counter,
                };

                for (i, param_name) in params.names.iter().enumerate() {
                    let val = func_translator.builder.block_params(entry_block)[i];
                    let var = func_translator.declare_local(param_name);
                    func_translator.builder.def_var(var, val);
                }

                for stmt in body {
                    func_translator.translate_statement(stmt);
                }
                let default_ret = func_translator.builder.ins().iconst(self.int, 0);
                func_translator.builder.ins().return_(&[default_ret]);
                func_translator.builder.finalize();

                let func_id = self
                    .module
                    .declare_function(&func_name, Linkage::Local, &new_ctx.func.signature)
                    .expect("Failed to declare function");
                self.module
                    .define_function(func_id, &mut new_ctx)
                    .expect("Failed to define function");
                self.module.clear_context(&mut new_ctx);
                self.module.finalize_definitions().unwrap();
                let func_ptr = self.module.get_finalized_function(func_id);

                let data_id = self
                    .module
                    .declare_data(&func_name, Linkage::Export, true, false)
                    .expect("Failed to declare global function data");
                let mut data_desc = DataDescription::new();
                data_desc.define_zeroinit(8);
                let _ = self.module.define_data(data_id, &data_desc);
                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                let ptr = {
                    let ins = self.builder.ins();
                    ins.symbol_value(self.int, local_id)
                };
                let iconst_val = {
                    let ins = self.builder.ins();
                    ins.iconst(self.int, func_ptr as i64)
                };
                {
                    let ins = self.builder.ins();
                    ins.store(MemFlags::trusted(), iconst_val, ptr, 0);
                }
            }
            _ => todo!("unimplemented {stmt:?}"),
        }
    }
    fn translate_expr(&mut self, expr: &Expression) -> Value {
        match expr {
            Expression::Nil => self.builder.ins().null(self.int),
            Expression::Boolean(v) => self.builder.ins().iconst(self.int, if *v { 1 } else { 0 }),
            Expression::Number(v) => match v {
                Number::Integer(i) => self.builder.ins().iconst(self.int, *i),
                Number::Float(f) => self.builder.ins().f64const(*f),
            },
            Expression::Var(name) => {   
                if let Some(var) = self.locals.get(name) {
                    self.builder.use_var(*var)
                } else {
                    self.builder.ins().iconst(self.int, 0)
                }
            }
            Expression::Unary(op, expr) => {
                let val = self.translate_expr(expr);
                match op {
                    UnaryOp::Minus => self.builder.ins().ineg(val),
                    UnaryOp::BitNot => self.builder.ins().bnot(val),
                    UnaryOp::Not => self.builder.ins().icmp_imm(IntCC::Equal, val, 0),
                    UnaryOp::Len => todo!(),
                }
            }
            Expression::Infix(op, exprs) => {
                match op {
                    InfixOp::Or => {
                        let mut iter = exprs.iter();
                        let first_val = self.translate_expr(iter.next().unwrap());
                        let merge_block = self.builder.create_block();
                        self.builder.append_block_param(merge_block, self.int);
    
                        let mut current_val = first_val;
                        for expr in iter {
                            let next_block = self.builder.create_block();
                            let truthy =
                                self.builder.ins().icmp_imm(IntCC::NotEqual, current_val, 0);
                            self.builder.ins().brif(truthy, merge_block, &[current_val], next_block, &[]);
                            self.builder.switch_to_block(next_block);
                            self.builder.seal_block(next_block);
                            current_val = self.translate_expr(expr);
                        }
                        self.builder.ins().jump(merge_block, &[current_val]);
                        self.builder.switch_to_block(merge_block);
                        self.builder.seal_block(merge_block);
                        let phi = self.builder.block_params(merge_block)[0];
                        phi
                    }
                    InfixOp::And => {
                        let mut iter = exprs.iter();
                        let first_val = self.translate_expr(iter.next().unwrap());
                        let merge_block = self.builder.create_block();
                        self.builder.append_block_param(merge_block, self.int);
    
                        let mut current_val = first_val;
                        for expr in iter {
                            let next_block = self.builder.create_block();
                            let falsy =
                                self.builder.ins().icmp_imm(IntCC::Equal, current_val, 0);
                            self.builder.ins().brif(falsy, merge_block, &[current_val], next_block, &[]);
                            self.builder.switch_to_block(next_block);
                            self.builder.seal_block(next_block);
                            current_val = self.translate_expr(expr);
                        }
                        self.builder.ins().jump(merge_block, &[current_val]);
                        self.builder.switch_to_block(merge_block);
                        self.builder.seal_block(merge_block);
                        let phi = self.builder.block_params(merge_block)[0];
                        phi
                    }
                    _ => {
                        let values: Vec<_> = exprs.iter().map(|e| self.translate_expr(e)).collect();
                        let mut iter = values.into_iter();
                        let mut e = iter.next().unwrap();
                        if *op == InfixOp::Concat {
                            let mut sig = self.module.make_signature();
                            sig.params.push(AbiParam::new(self.int));
                            sig.params.push(AbiParam::new(self.int));
                            sig.returns.push(AbiParam::new(self.int));
                            let func_id = self.module
                                .declare_function("moonshine_concat", Linkage::Import, &sig)
                                .expect("Failed to declare concatenation function");
                            let concat_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                            for val in iter {
                                let call = self.builder.ins().call(concat_callee, &[e, val]);
                                e = self.builder.inst_results(call)[0];
                            }
                        } else {
                            for val in iter {
                                e = match op {
                                    InfixOp::Add => self.builder.ins().iadd(e, val),
                                    InfixOp::Sub => self.builder.ins().isub(e, val),
                                    InfixOp::Mul => self.builder.ins().imul(e, val),
                                    InfixOp::Div => self.builder.ins().sdiv(e, val),
                                    InfixOp::FloorDiv => self.builder.ins().udiv(e, val),
                                    InfixOp::Mod => self.builder.ins().srem(e, val),
                                    InfixOp::Less => self.builder.ins().icmp(IntCC::SignedLessThan, e, val),
                                    InfixOp::LessEq => self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, e, val),
                                    InfixOp::Greater => self.builder.ins().icmp(IntCC::SignedGreaterThan, e, val),
                                    InfixOp::GreaterEq => self.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, e, val),
                                    InfixOp::Eq => self.builder.ins().icmp(IntCC::Equal, e, val),
                                    InfixOp::NotEq => self.builder.ins().icmp(IntCC::NotEqual, e, val),
                                    _ => unreachable!("Unexpected operator"),
                                }
                            }
                        }
                        e
                    }
                }
            }
            Expression::String(bytes) => {
                let data_name = format!("str_{}", self.string_counter);
                self.string_counter += 1;
                
                let data_id = self.module
                    .declare_data(
                        &data_name,
                        Linkage::Local,
                        true, 
                        false, 
                    )
                    .map_err(|e| panic!("Failed to declare data: {}", e))
                    .unwrap();
                let mut data_desc = DataDescription::new();
                data_desc.define(bytes.as_ref().into());
                
                self.module
                    .define_data(data_id, &data_desc)
                    .map_err(|e| panic!("Failed to define data: {}", e))
                    .unwrap(); 
                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                let ptr = self.builder.ins().symbol_value(self.int, local_id);
                ptr
            }
            Expression::FunctCall(call) => {
                self.translate_function_call(call)
            }
            Expression::Field(table, field) => {
                if let Expression::Var(table_name) = &**table {
                    if table_name == "_G" {
                        let field_name = field.as_str();
                        let mut data_desc = DataDescription::new();
                        data_desc.define_zeroinit(8); 
                        let data_id = self.module
                            .declare_data(
                                field_name, 
                                Linkage::Export,
                                true,  
                                false, 
                            )
                            .expect("Failed to declare global variable");
                        
                        self.module
                            .define_data(data_id, &data_desc)
                            .expect("Failed to define global variable data");
                        
                        let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                        let ptr = self.builder.ins().symbol_value(self.int, local_id);
                        return self.builder.ins().load(self.int, MemFlags::trusted(), ptr, 0);
                    }
                }
                self.builder.ins().iconst(self.int, 0)
            }
            Expression::Table(fields) => {
                if !fields.is_empty() {
                    todo!("Table with fields not yet implemented: {:?}", fields);
                }
                let mut sig = self.module.make_signature();
                sig.returns.push(AbiParam::new(self.int));
                let table_helper_id = self.module
                    .declare_function("moonshine_newtable", Linkage::Import, &sig)
                    .expect("Failed to declare newtable helper function");
                let newtable_callee = self.module.declare_func_in_func(table_helper_id, self.builder.func);
                let call = self.builder.ins().call(newtable_callee, &[]);
                let table_ptr = self.builder.inst_results(call)[0];
                table_ptr
            }
            _ => todo!("Unsupported expression {expr:?}"),
        }
    }
    fn declare_local(&mut self, name: &str) -> Variable {
        assert!(!self.locals.contains_key(name));
        let idx = self.locals.len();
        let var = Variable::new(idx);
        self.locals.insert(name.to_string(), var);
        self.builder.declare_var(var, self.int);
        var
    }
    fn translate_function_call(&mut self, call: &FunctionCall) -> Value {
        
        match &call.prefix {
            Expression::Field(table, field) => {
                self.translate_expr(&table); 
            }
            _ => {
                self.translate_expr(&call.prefix);
            }
        }
        for arg in &call.args {
            self.translate_expr(arg);
        }
        self.builder.ins().iconst(self.int, 0)
    }
}