use std::collections::HashMap;
use crate::ast::{Expression, FunctionCall, InfixOp, Number, Statement, UnaryOp};
use crate::Source;
use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, Linkage, Module, DataId};
use crate::runtime::{lua_len, lua_newtable, lua_settable, lua_concat, lua_next, lua_gettable};
use std::collections::HashSet;
use cranelift::codegen::ir::Opcode;
use cranelift::frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift::codegen::ir::{Block, Type};
use cranelift::codegen::verify_function;
use cranelift::codegen::Context;
use cranelift_module::ModuleError;
use std::collections::VecDeque;

pub struct JIT {
    builder_context: FunctionBuilderContext,
    ctx: codegen::Context,
    data_description: DataDescription,
    module: JITModule,
    global_data: HashMap<String, DataId>,
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
        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        builder.symbol("lua_len", lua_len as *const u8);
        builder.symbol("lua_newtable", lua_newtable as *const u8);
        builder.symbol("lua_settable", lua_settable as *const u8);
        builder.symbol("lua_concat", lua_concat as *const u8);
        builder.symbol("lua_next", lua_next as *const u8);
        builder.symbol("lua_gettable", lua_gettable as *const u8);
        let module = JITModule::new(builder);
        let builder_context = FunctionBuilderContext::new();
        let ctx = module.make_context();
        let data_description = DataDescription::new();
        Self {
            builder_context,
            ctx,
            data_description,
            module,
            global_data: HashMap::new(),
        }
    }

    pub fn compile(&mut self, source: &Source) -> Result<(), String> {
        self.compile_fn(&[], &source.block)?;
        Ok(())
    }

    pub fn compile_fn(&mut self, params: &[String], block: &[Statement]) -> Result<*const u8, String> {
        let int = self.module.target_config().pointer_type();
        
        for _ in params {
            self.ctx.func.signature.params.push(AbiParam::new(int));
        }
        self.ctx.func.signature.returns.push(AbiParam::new(int));
        
        let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.builder_context);
        let mut block_mgr = BlockManager::new(int);
        
        let entry_block = block_mgr.create_block(&mut builder);
        builder.append_block_params_for_function_params(entry_block);
        block_mgr.switch_to_block(&mut builder, entry_block);
        
        let mut translator = Translator {
            int,
            builder,
            block_mgr,
            locals: HashMap::new(),
            scopes: Vec::new(),
            next_var: 0,
            module: &mut self.module,
            string_counter: 0,
            globals: &mut self.global_data,
            terminated: false,
        };
        
        for (i, param) in params.iter().enumerate() {
            let val = translator.builder.block_params(entry_block)[i];
            let var = translator.declare_local(param);
            translator.builder.def_var(var, val);
        }
        
        for stmt in block {
            // If the translator is marked terminated or the builder block is unreachable, stop processing further statements.
            if translator.terminated || translator.builder.is_unreachable() {
                break;
            }
            translator.translate_statement(stmt);
        }

        // Ensure the current block is properly terminated.
        if !translator.builder.is_unreachable() {
            let default_ret = translator.builder.ins().iconst(translator.int, 0);
            translator.builder.ins().return_(&[default_ret]);
            eprintln!("Debug [compile_fn]: Added default return instruction.");
        }

        // Finalize all blocks properly
        translator.block_mgr.finalize_blocks(&mut translator.builder);

        // Finalize the builder to emit the function.
        translator.builder.finalize();
        
        let id = self
            .module
            .declare_function("main", Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        
        self.module
            .define_function(id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
        
        self.module.clear_context(&mut self.ctx);
        
        self.module.finalize_definitions()
            .map_err(|e| format!("Failed to finalize definitions: {}", e))?;
        
        let code = self.module.get_finalized_function(id);

        // After building the function:
        match verify_function(&self.ctx.func, self.module.isa()) {
            Ok(_) => eprintln!("Function verified successfully."),
            Err(e) => return Err(format!("Verification failed: {:?}", e)),
        }
        
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
    block_mgr: BlockManager,
    locals: HashMap<String, Vec<Variable>>,
    scopes: Vec<Vec<String>>,
    next_var: usize,
    module: &'a mut JITModule,
    string_counter: usize,
    globals: &'a mut HashMap<String, DataId>,
    terminated: bool,
}
impl Translator<'_> {
    fn declare_global(&mut self, name: &str, initializer: Option<&[u8]>) -> DataId {
        if let Some(&data_id) = self.globals.get(name) {
            data_id
        } else {
            let data_id = self
                .module
                .declare_data(name, Linkage::Export, true, false)
                .expect("Failed to declare global variable data");
            let mut data_desc = DataDescription::new();
            if let Some(init) = initializer {
                data_desc.define(init.into());
            } else {
                data_desc.define_zeroinit(8);
            }
            self.module
                .define_data(data_id, &data_desc)
                .expect("Failed to define global variable data");
            self.globals.insert(name.to_string(), data_id);
            data_id
        }
    }
    fn ensure_valid_block(&mut self, add_params: bool) {
        // Check if we need a new block
        let need_new_block = match self.block_mgr.get_current_block() {
            None => true,
            Some(block) => {
                self.builder.is_unreachable() ||
                self.block_mgr.is_block_inserted(&self.builder, block)
            }
        };

        if need_new_block {
            // Create a new block and switch to it
            let new_block = self.block_mgr.create_block(&mut self.builder);
            if add_params {
                self.builder.append_block_params_for_function_params(new_block);
            }
            
            self.block_mgr.switch_to_block(&mut self.builder, new_block);
        }
    }
    fn translate_statement(&mut self, stmt: &Statement) {
        if self.terminated {
            eprintln!("Debug: Skipping statement because translator marked terminated: {:?}", stmt);
            return;
        }

        // Ensure we have a valid block before processing the statement
        self.ensure_valid_block(true);
        
        match stmt {
            Statement::If { ref ifcases, ref elsecase } => {
                let merge_block = self.block_mgr.create_block(&mut self.builder);
                let mut branch_non_terminated = false;
                let mut all_branches_terminated = true;  // Track if all branches terminate

                let mut current_test_block = self.block_mgr.get_current_block().unwrap();

                // Process each if‑case.
                for (cond, block) in ifcases {
                    // Evaluate condition in current block
                    let cond_value = self.translate_expr(cond);
                    let then_block = self.block_mgr.create_block(&mut self.builder);
                    let else_block = self.block_mgr.create_block(&mut self.builder);

                    // Add the conditional branch
                    self.builder.ins().brif(cond_value, then_block, &[], else_block, &[]);
                    self.block_mgr.seal(&mut self.builder, current_test_block);

                    // Process the "then" block
                    self.block_mgr.switch_to_block(&mut self.builder, then_block);
                    self.terminated = false;
                    self.enter_scope();
                    
                    for stmt in block {
                        if self.builder.is_unreachable() || self.terminated {
                            break;
                        }
                        self.translate_statement(stmt);
                    }
                    
                    let then_terminated = self.terminated;
                    self.exit_scope();
                    
                    // Update all_branches_terminated
                    all_branches_terminated &= then_terminated;
                    
                    if !then_terminated && !self.builder.is_unreachable() {
                        self.block_mgr.jump_to_block(&mut self.builder, merge_block);
                        branch_non_terminated = true;
                    }
                    self.block_mgr.seal(&mut self.builder, then_block);

                    // Continue with the else block
                    self.block_mgr.switch_to_block(&mut self.builder, else_block);
                    current_test_block = else_block;
                }

                // Process the final else block
                self.enter_scope();
                self.terminated = false;
                
                for stmt in elsecase {
                    if self.builder.is_unreachable() {
                        break;
                    }
                    self.translate_statement(stmt);
                    if self.terminated {
                        break;
                    }
                }
                
                let else_terminated = self.terminated;
                self.exit_scope();
                
                // Update all_branches_terminated with else block
                all_branches_terminated &= else_terminated;

                if !else_terminated && !self.builder.is_unreachable() {
                    self.block_mgr.jump_to_block(&mut self.builder, merge_block);
                    branch_non_terminated = true;
                }
                self.block_mgr.seal(&mut self.builder, current_test_block);

                if branch_non_terminated {
                    self.block_mgr.switch_to_block(&mut self.builder, merge_block);
                    self.block_mgr.seal(&mut self.builder, merge_block);
                    self.terminated = false;
                } else {
                    self.terminated = all_branches_terminated;
                }
            }
            Statement::While {
                ref cond,
                ref block,
            } => {
                eprintln!("Debug: Processing While statement");
                let header_block = self.builder.create_block();
                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();

                if !self.builder.is_unreachable() {
                    self.block_mgr.jump_to_block(&mut self.builder, header_block);
                }

                self.builder.switch_to_block(header_block);

                let cond_value = self.translate_expr(cond);

                self.builder
                    .ins()
                    .brif(cond_value, body_block, &[], exit_block, &[]);

                self.block_mgr.seal(&mut self.builder, header_block);

                self.builder.switch_to_block(body_block);
                self.enter_scope();
                for stmt in block {
                    if self.builder.is_unreachable() {
                        break;
                    }
                    self.translate_statement(stmt);
                }
                if !self.builder.is_unreachable() {
                    self.builder.ins().jump(header_block, &[]);
                }
                self.exit_scope();

                self.builder.switch_to_block(exit_block);

                self.block_mgr.seal(&mut self.builder, body_block);
                self.block_mgr.seal(&mut self.builder, exit_block);
            }
            Statement::Repeat {
                ref block,
                ref cond,
            } => {
                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();
                if !self.builder.is_unreachable() {
                    self.builder.ins().jump(body_block, &[]);
                }
                self.builder.switch_to_block(body_block);
                self.enter_scope();
                for stmt in block {
                    if self.builder.is_unreachable() {
                        break;
                    }
                    self.translate_statement(stmt);
                }
                let cond_value = self.translate_expr(cond);
                if !self.builder.is_unreachable() {
                    self.builder
                        .ins()
                        .brif(cond_value, exit_block, &[], body_block, &[]);
                }
                self.exit_scope();
                self.builder.switch_to_block(exit_block);
                self.block_mgr.seal(&mut self.builder, body_block);
                self.block_mgr.seal(&mut self.builder, exit_block);
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
                            if let Some(stack) = self.locals.get(name) {
                                self.builder.def_var(*stack.last().unwrap(), val);
                            } else {
                                let data_id = self.declare_global(name, None);
                                
                                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                                let ptr = self.builder.ins().symbol_value(self.int, local_id);
                                self.builder.ins().store(MemFlags::trusted(), val, ptr, 0);
                            }
                        }
                        Expression::Field(table, field_name) => {
                            if let Expression::Var(table_name) = &**table {
                                if table_name == "_G" {
                                    let data_id = self.declare_global(field_name, None);
                                    
                                    let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                                    let ptr = self.builder.ins().symbol_value(self.int, local_id);
                                    self.builder.ins().store(MemFlags::trusted(), val, ptr, 0);
                                    continue;
                                }
                            }
                            let table_val = self.translate_expr(table);
                            
                            let data_name = format!("str_{}", self.string_counter);
                            self.string_counter += 1;
                            let data_id = self.declare_global(&data_name, Some(field_name.as_bytes()));
                            let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                            let index_val = self.builder.ins().symbol_value(self.int, local_id);

                            let mut sig = self.module.make_signature();
                            sig.params.push(AbiParam::new(self.int));
                            sig.params.push(AbiParam::new(self.int));
                            sig.params.push(AbiParam::new(self.int));
                            sig.returns.push(AbiParam::new(self.int));
                            
                            let func_id = self.module
                                .declare_function("lua_settable", Linkage::Import, &sig)
                                .expect("Failed to declare settable helper function");
                            let settable_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                            self.builder.ins().call(settable_callee, &[table_val, index_val, val]);
                        }
                        Expression::Index(table_expr, index_expr) => {
                            let table_val = self.translate_expr(table_expr);
                            let index_val = self.translate_expr(index_expr);
                            
                            let mut sig = self.module.make_signature();
                            sig.params.push(AbiParam::new(self.int));
                            sig.params.push(AbiParam::new(self.int));
                            sig.params.push(AbiParam::new(self.int));
                            sig.returns.push(AbiParam::new(self.int));
                            
                            let func_id = self.module
                                .declare_function("lua_settable", Linkage::Import, &sig)
                                .expect("Failed to declare settable helper function");
                            let settable_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                            self.builder.ins().call(settable_callee, &[table_val, index_val, val]);
                        }
                        _ => todo!("not implemented assignment for {:?}", var),
                    }
                }
            }
            Statement::Return(ref exprs) => {
                eprintln!("Debug [RETURN]: Starting return statement processing");
                
                // Now handle the return
                let return_val = if exprs.is_empty() {
                    self.builder.ins().iconst(self.int, 0)
                } else if exprs.len() == 1 {
                    self.translate_expr(&exprs[0])
                } else {
                    // TODO: Handle multiple return values
                    self.translate_expr(&exprs[0])
                };
                
                // Add the return instruction if possible
                if !self.builder.is_unreachable() {
                    self.builder.ins().return_(&[return_val]);
                    eprintln!("Debug [RETURN]: Added return instruction.");
                }
                
                // Seal the current block if needed
                if let Some(current) = self.block_mgr.get_current_block() {
                    if !self.block_mgr.sealed_blocks.contains(&(current.index() as u32)) {
                        self.block_mgr.seal(&mut self.builder, current);
                        eprintln!("Debug [RETURN]: Sealed current block {:?}", current);
                    }
                }
                
                // Clear the current block so that later ensure_valid_block calls don't try to switch from a terminated block
                self.block_mgr.current_block = None;
                
                eprintln!("Debug [RETURN]: Block marked as terminated");
                self.terminated = true;
            }
            Statement::FunctCall(call) => {
                let _ = self.translate_function_call(call);
            }
            Statement::Do(block) => {
                self.enter_scope();
                for stmt in block {
                    if self.builder.is_unreachable() {
                        break;
                    }
                    self.translate_statement(stmt);
                }
                self.exit_scope();
            }
            Statement::Function { ref name, ref params, ref body } => {
                let func_name = name.qname.join(".");
    
                let mut new_ctx = self.module.make_context();
                for _ in &params.names {
                    new_ctx.func.signature.params.push(AbiParam::new(self.int));
                }
                new_ctx.func.signature.returns.push(AbiParam::new(self.int));
    
                let mut new_builder_context = FunctionBuilderContext::new();
                let new_string_counter = {
                    let mut builder =
                        FunctionBuilder::new(&mut new_ctx.func, &mut new_builder_context);
    
                    let entry_block = builder.create_block();
                    builder.append_block_params_for_function_params(entry_block);
                    builder.switch_to_block(entry_block);
                    builder.seal_block(entry_block);
    
                    let mut func_translator = Translator {
                        int: self.int,
                        builder,
                        block_mgr: BlockManager::new(self.int),
                        locals: HashMap::new(),
                        scopes: vec![Vec::new()],
                        next_var: 0,
                        module: self.module,
                        string_counter: self.string_counter,
                        globals: self.globals,
                        terminated: false,
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
    
                    func_translator.string_counter
                };
                self.string_counter = new_string_counter;
    
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
    
                let global_fn_name = format!("__fn_ptr_{}", func_name);
                let data_id = self.declare_global(&global_fn_name, None);
    
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
            Statement::ForEach { vars, exprs, block } => {
                if exprs.len() == 1 {
                    if let Expression::FunctCall(call) = &exprs[0] {
                        if let Expression::Var(func_name) = &call.prefix {
                            if func_name == "pairs" {
                                if let Some(table_expr) = call.args.get(0) {
                                    let table_val = self.translate_expr(table_expr);
                                    let initial_key = self.builder.ins().iconst(self.int, 0);
                                    let iter_key_var = self.declare_local("iter_key_temp");
                                    self.builder.def_var(iter_key_var, initial_key);
                                    
                                    let loop_header = self.builder.create_block();
                                    let loop_body = self.builder.create_block();
                                    let loop_exit = self.builder.create_block();
                                    
                                    self.builder.append_block_param(loop_body, self.int);
                                    
                                    if !self.builder.is_unreachable() {
                                        self.block_mgr.jump_to_block(&mut self.builder, loop_header);
                                    }
                                    
                                    self.builder.switch_to_block(loop_header);
                                    
                                    let current_key = self.builder.use_var(iter_key_var);
                                    
                                    let mut sig = self.module.make_signature();
                                    sig.params.push(AbiParam::new(self.int));
                                    sig.params.push(AbiParam::new(self.int));
                                    sig.returns.push(AbiParam::new(self.int));
                                    
                                    let func_id = self.module
                                        .declare_function("lua_next", Linkage::Import, &sig)
                                        .expect("Failed to declare lua_next helper function");
                                    let lua_next_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                                    
                                    let call_inst = self.builder.ins().call(lua_next_callee, &[table_val, current_key]);
                                    let next_key = self.builder.inst_results(call_inst)[0];
                                    
                                    let cmp = self.builder.ins().icmp_imm(IntCC::Equal, next_key, 0);
                                    self.builder.ins().brif(cmp, loop_exit, &[], loop_body, &[next_key]);
                                    
                                    self.builder.switch_to_block(loop_body);
                                    
                                    let loop_current_key = self.builder.block_params(loop_body)[0];
                                    
                                    if let Some(loop_var_name) = vars.get(0) {
                                        let loop_var = self.declare_local(loop_var_name);
                                        self.builder.def_var(loop_var, loop_current_key);
                                    }
                                    if let Some(loop_var_value) = vars.get(1) {
                                        let mut sig = self.module.make_signature();
                                        sig.params.push(AbiParam::new(self.int));
                                        sig.params.push(AbiParam::new(self.int));
                                        sig.returns.push(AbiParam::new(self.int));
                                        
                                        let func_id = self.module
                                            .declare_function("lua_gettable", Linkage::Import, &sig)
                                            .expect("Failed to declare lua_gettable helper function");
                                        let lua_gettable_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                                        let gettable_call = self.builder.ins().call(lua_gettable_callee, &[table_val, loop_current_key]);
                                        let value = self.builder.inst_results(gettable_call)[0];
                                        
                                        let loop_var = self.declare_local(loop_var_value);
                                        self.builder.def_var(loop_var, value);
                                    }
                                    
                                    self.enter_scope();
                                    for stmt in block {
                                        if self.builder.is_unreachable() {
                                            break;
                                        }
                                        self.translate_statement(stmt);
                                    }
                                    self.exit_scope();
                                    
                                    self.builder.def_var(iter_key_var, next_key);
                                    
                                    if !self.builder.is_unreachable() {
                                        self.block_mgr.jump_to_block(&mut self.builder, loop_header);
                                    }
                                    
                                    self.builder.switch_to_block(loop_exit);
                                    self.block_mgr.seal(&mut self.builder, loop_exit);
                                }
                            }
                        }
                    }
                }
                todo!("ForEach not implemented for given expression");
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
                Number::Float(f) => self.builder.ins().iconst(self.int, *f as i64),
            },
            Expression::Var(name) => {   
                if let Some(stack) = self.locals.get(name) {
                    if let Some(&var) = stack.last() {
                        self.builder.use_var(var)
                    } else {
                        self.builder.ins().iconst(self.int, 0)
                    }
                } else {
                    self.builder.ins().iconst(self.int, 0)
                }
            }
            Expression::Unary(op, expr) => {
                let val = self.translate_expr(expr);
                match op {
                    UnaryOp::Minus => self.builder.ins().ineg(val),
                    UnaryOp::BitNot => self.builder.ins().bnot(val),
                    UnaryOp::Not => {
                        let cmp = self.builder.ins().icmp_imm(IntCC::Equal, val, 0);
                        self.builder.ins().uextend(self.int, cmp)
                    },
                    UnaryOp::Len => {
                        let mut sig = self.module.make_signature();
                        sig.params.push(AbiParam::new(self.int));
                        sig.returns.push(AbiParam::new(self.int));
                        let func_id = self.module
                            .declare_function("lua_len", Linkage::Import, &sig)
                            .expect("Failed to declare lua_len helper function");
                        let callee = self.module.declare_func_in_func(func_id, self.builder.func);
                        let call = self.builder.ins().call(callee, &[val]);
                        self.builder.inst_results(call)[0]
                    },
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
                        if !self.builder.is_unreachable() {
                            self.builder.ins().jump(merge_block, &[current_val]);
                        }
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
                        if !self.builder.is_unreachable() {
                            self.builder.ins().jump(merge_block, &[current_val]);
                        }
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
                                .declare_function("lua_concat", Linkage::Import, &sig)
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
                                    InfixOp::Less => {
                                        let cmp = self.builder.ins().icmp(IntCC::SignedLessThan, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
                                    InfixOp::LessEq => {
                                        let cmp = self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
                                    InfixOp::Greater => {
                                        let cmp = self.builder.ins().icmp(IntCC::SignedGreaterThan, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
                                    InfixOp::GreaterEq => {
                                        let cmp = self.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
                                    InfixOp::Eq => {
                                        let cmp = self.builder.ins().icmp(IntCC::Equal, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
                                    InfixOp::NotEq => {
                                        let cmp = self.builder.ins().icmp(IntCC::NotEqual, e, val);
                                        self.builder.ins().uextend(self.int, cmp)
                                    },
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
                
                let data_id = self.declare_global(&data_name, Some(bytes.as_ref()));
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
                        let data_id = self.declare_global(field_name, None);
                        let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                        let ptr = self.builder.ins().symbol_value(self.int, local_id);
                        return self.builder.ins().load(self.int, MemFlags::trusted(), ptr, 0);
                    }
                }
                self.builder.ins().iconst(self.int, 0)
            }
            Expression::Index(table, index) => {
                let table_val = self.translate_expr(table);
                let index_val = self.translate_expr(index);
                let mut sig = self.module.make_signature();
                sig.params.push(AbiParam::new(self.int));
                sig.params.push(AbiParam::new(self.int));
                sig.returns.push(AbiParam::new(self.int));
                let func_id = self.module
                    .declare_function("lua_gettable", Linkage::Import, &sig)
                    .expect("Failed to declare gettable helper function");
                let gettable_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                let call_inst = self.builder.ins().call(gettable_callee, &[table_val, index_val]);
                self.builder.inst_results(call_inst)[0]
            }
            Expression::Table(fields) => {
                let mut sig = self.module.make_signature();
                sig.returns.push(AbiParam::new(self.int));
                let table_helper_id = self.module
                    .declare_function("lua_newtable", Linkage::Import, &sig)
                    .expect("Failed to declare newtable helper function");
                let newtable_callee = self.module.declare_func_in_func(table_helper_id, self.builder.func);
                let call = self.builder.ins().call(newtable_callee, &[]);
                let table_ptr = self.builder.inst_results(call)[0];

                if !fields.is_empty() {
                    for field in fields {
                        match field {
                            crate::ast::Field::Named(name, expr) => {
                                let value = self.translate_expr(expr);
                                let data_name = format!("str_{}", self.string_counter);
                                self.string_counter += 1;
                                let data_id = self.declare_global(&data_name, Some(name.as_bytes()));
                                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                                let key_ptr = self.builder.ins().symbol_value(self.int, local_id);

                                let mut set_sig = self.module.make_signature();
                                set_sig.params.push(AbiParam::new(self.int));
                                set_sig.params.push(AbiParam::new(self.int));
                                set_sig.params.push(AbiParam::new(self.int));
                                set_sig.returns.push(AbiParam::new(self.int));
                                
                                let settable_id = self.module
                                    .declare_function("lua_settable", Linkage::Import, &set_sig)
                                    .expect("Failed to declare settable helper function");
                                let settable_callee = self.module.declare_func_in_func(settable_id, self.builder.func);
                                self.builder.ins().call(settable_callee, &[table_ptr, key_ptr, value]);
                            },
                            _ => {
                                todo!("Only Named table fields are implemented. Field: {:?}", field)
                            }
                        }
                    }
                }

                table_ptr
            }
            Expression::FunctDef(params, body) => {
                let lambda_name = format!("lambda_{}", self.string_counter);
                self.string_counter += 1;
                
                let mut new_ctx = self.module.make_context();
                for _ in &params.names {
                    new_ctx.func.signature.params.push(AbiParam::new(self.int));
                }
                new_ctx.func.signature.returns.push(AbiParam::new(self.int));
                let mut new_builder_context = FunctionBuilderContext::new();
                {
                    let mut builder = FunctionBuilder::new(&mut new_ctx.func, &mut new_builder_context);
                    let entry_block = builder.create_block();
                    builder.append_block_params_for_function_params(entry_block);
                    builder.switch_to_block(entry_block);
                    builder.seal_block(entry_block);
                    let mut func_translator = Translator {
                        int: self.int,
                        builder,
                        block_mgr: BlockManager::new(self.int),
                        locals: HashMap::new(),
                        scopes: vec![Vec::new()],
                        next_var: 0,
                        module: self.module,
                        string_counter: self.string_counter,
                        globals: self.globals,
                        terminated: false,
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
                    self.string_counter = func_translator.string_counter;
                }
                let func_id = self
                    .module
                    .declare_function(&lambda_name, Linkage::Local, &new_ctx.func.signature)
                    .expect("Failed to declare lambda function");
                self.module
                    .define_function(func_id, &mut new_ctx)
                    .expect("Failed to define lambda function");
                self.module.clear_context(&mut new_ctx);
                self.module.finalize_definitions().unwrap();
                let fn_ptr = self.module.get_finalized_function(func_id);
                self.builder.ins().iconst(self.int, fn_ptr as i64)
            }
            _ => todo!("Unsupported expression {expr:?}"),
        }
    }
    fn enter_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn exit_scope(&mut self) {
        if let Some(vars) = self.scopes.pop() {
            for name in vars {
                if let Some(stack) = self.locals.get_mut(&name) {
                    stack.pop();
                    if stack.is_empty() {
                        self.locals.remove(&name);
                    }
                }
            }
        }
    }

    fn declare_local(&mut self, name: &str) -> Variable {
        let var = Variable::new(self.next_var);
        self.next_var += 1;
        self.locals.entry(name.to_string()).or_default().push(var);
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(name.to_string());
        } else {
            self.scopes.push(vec![name.to_string()]);
        }
        self.builder.declare_var(var, self.int);
        var
    }
    fn translate_function_call(&mut self, call: &FunctionCall) -> Value {
        let func_val = match &call.prefix {
            Expression::Field(table, field) => {
                let table_val = self.translate_expr(table);

                let data_name = format!("str_{}", self.string_counter);
                self.string_counter += 1;
                let data_id = self.declare_global(&data_name, Some(field.as_bytes()));
                let local_id = self.module.declare_data_in_func(data_id, self.builder.func);
                let field_ptr = self.builder.ins().symbol_value(self.int, local_id);

                let mut sig = self.module.make_signature();
                sig.params.push(AbiParam::new(self.int));
                sig.params.push(AbiParam::new(self.int));
                sig.returns.push(AbiParam::new(self.int));

                let func_id = self.module
                    .declare_function("lua_gettable", Linkage::Import, &sig)
                    .expect("Failed to declare gettable helper function");
                let gettable_callee = self.module.declare_func_in_func(func_id, self.builder.func);
                let call_inst = self.builder.ins().call(gettable_callee, &[table_val, field_ptr]);
                self.builder.inst_results(call_inst)[0]
            }
            _ => self.translate_expr(&call.prefix),
        };

        let args: Vec<_> = call.args.iter()
            .map(|arg| self.translate_expr(arg))
            .collect();

        let mut sig = self.module.make_signature();
        for _ in 0..args.len() {
            sig.params.push(AbiParam::new(self.int));
        }
        sig.returns.push(AbiParam::new(self.int));

        let sig_ref = self.builder.import_signature(sig);
        let call_inst = self.builder.ins().call_indirect(sig_ref, func_val, &args);
        self.builder.inst_results(call_inst)[0]
    }
    fn debug_block_state(&self, location: &str) {
        eprintln!("Debug: Block state at {}", location);
        eprintln!("  Is unreachable: {}", self.builder.is_unreachable());
        // Add any other relevant state information
    }
}

/// The BlockManager tracks all created blocks and ensures they are properly sealed.
/// Sealed blocks are tracked by their unique block index (converted to u32).
pub struct BlockManager {
    /// All blocks created so far.
    block_stack: Vec<Block>,
    /// Blocks that have been sealed (tracked by the block's index as a u32).
    sealed_blocks: HashSet<u32>,
    /// The block currently "open" for emitting instructions.
    current_block: Option<Block>,
    int: Type,
}

impl BlockManager {
    pub fn new(int_type: Type) -> Self {
        Self {
            block_stack: Vec::new(),
            sealed_blocks: HashSet::new(),
            current_block: None,
            int: int_type,
        }
    }

    /// Create a new block.
    pub fn create_block(&mut self, builder: &mut FunctionBuilder) -> Block {
        let block = builder.create_block();
        block
    }

    /// Switch to a new block.
    ///
    /// If there is a current block that is not terminated, an unconditional jump is
    /// inserted (with zero-valued parameters) and that block is sealed. Then the builder
    /// is switched to the new block.
    pub fn switch_to_block(&mut self, builder: &mut FunctionBuilder, new_block: Block) {
        // Handle current block if it exists and isn't sealed
        if let Some(curr) = self.current_block {
            if !self.sealed_blocks.contains(&(curr.index() as u32)) {
                if !self.is_block_terminated(builder, curr) {
                    let param_types: Vec<Type> = builder.func.dfg.block_params(new_block)
                        .iter()
                        .map(|&v| builder.func.dfg.value_type(v))
                        .collect();
                    let params: Vec<Value> = param_types
                        .iter()
                        .map(|&ty| {
                            if ty.is_int() {
                                builder.ins().iconst(ty, 0)
                            } else if ty.is_float() {
                                builder.ins().f64const(0.0)
                            } else {
                                panic!(
                                    "Unsupported block parameter type in switch_to_block: {:?}",
                                    ty
                                );
                            }
                        })
                        .collect();
                    builder.ins().jump(new_block, &params);
                }
                builder.seal_block(curr);
                eprintln!("BlockManager: Sealed current block {:?}", curr);
                self.sealed_blocks.insert(curr.index() as u32);
            }
            // Clear current_block before switching
            self.current_block = None;
        }

        eprintln!("BlockManager: Switching to new block {:?}", new_block);
        builder.switch_to_block(new_block);
        self.current_block = Some(new_block);
        if !self.block_stack.contains(&new_block) {
            self.block_stack.push(new_block);
            eprintln!("BlockManager: Pushed new block {:?}", new_block);
        }
    }

    /// Seal the given block.
    ///
    /// If the block is the current one and not terminated, a default return is inserted.
    /// If it is already sealed (by its index), nothing more is done.
    pub fn seal(&mut self, builder: &mut FunctionBuilder, block: Block) {
        eprintln!("BlockManager: Sealing block {:?}", block);
        if self.sealed_blocks.contains(&(block.index() as u32)) {
            eprintln!("BlockManager: Block {:?} already sealed", block);
            return;
        }
        
        // If this is the current block and it's not terminated, add a return
        if self.current_block == Some(block) && !self.is_block_terminated(builder, block) {
            let ret_val = builder.ins().iconst(self.int, 0);
            builder.ins().return_(&[ret_val]);
        }
        
        builder.seal_block(block);
        eprintln!("BlockManager: Block {:?} sealed", block);
        self.sealed_blocks.insert(block.index() as u32);
        
        // Clear current_block if we just sealed it
        if self.current_block == Some(block) {
            eprintln!("BlockManager: Clearing current block after sealing");
            self.current_block = None;
        }
    }

    /// Insert an unconditional jump from the current block to the target block,
    /// then seal the current block.
    pub fn jump_to_block(&mut self, builder: &mut FunctionBuilder, target: Block) {
        if let Some(curr) = self.current_block {
            if !self.sealed_blocks.contains(&(curr.index() as u32)) {
                eprintln!(
                    "BlockManager: Jumping from current block {:?} to target block {:?}",
                    curr, target
                );
                if !self.is_block_terminated(builder, curr) {
                    let param_types: Vec<Type> = builder
                        .func
                        .dfg
                        .block_params(target)
                        .iter()
                        .map(|&v| builder.func.dfg.value_type(v))
                        .collect();
                    let args: Vec<Value> = param_types
                        .iter()
                        .map(|&ty| {
                            if ty.is_int() {
                                builder.ins().iconst(ty, 0)
                            } else if ty.is_float() {
                                builder.ins().f64const(0.0)
                            } else {
                                panic!(
                                    "Unsupported block parameter type in jump_to_block: {:?}",
                                    ty
                                );
                            }
                        })
                        .collect();
                    eprintln!("BlockManager: Inserting jump with args {:?}", args);
                    builder.ins().jump(target, &args);
                }
                builder.seal_block(curr);
                eprintln!("BlockManager: Sealed current block {:?}", curr);
                self.sealed_blocks.insert(curr.index() as u32);
            }
            // Clear current_block now that we've finished with it
            self.current_block = None;
        }
    }

    /// Finalize all pending blocks by sealing any that are not yet sealed.
    pub fn finalize_blocks(&mut self, builder: &mut FunctionBuilder) {
        eprintln!(
            "BlockManager: Finalizing blocks, block_stack = {:?}",
            self.block_stack
        );
        for block in self.block_stack.clone() {
            if !self.sealed_blocks.contains(&(block.index() as u32)) {
                eprintln!("BlockManager: Finalizing block {:?}", block);
                self.seal(builder, block);
            }
        }
        self.block_stack.clear();
        self.current_block = None;
        self.sealed_blocks.clear();
        eprintln!("BlockManager: Finalization complete");
    }

    /// Check whether a given block is terminated (i.e. its last instruction is a terminator).
    pub fn is_block_terminated(&self, builder: &FunctionBuilder, block: Block) -> bool {
        if let Some(inst) = builder.func.layout.last_inst(block) {
            builder.func.dfg.insts[inst].opcode().is_terminator()
        } else {
            false
        }
    }

    /// Return the current block (if any).
    pub fn get_current_block(&self) -> Option<Block> {
        self.current_block
    }

    /// Check if a block is already inserted in the layout.
    pub fn is_block_inserted(&self, builder: &FunctionBuilder, block: Block) -> bool {
        builder.func.layout.is_block_inserted(block)
    }
}