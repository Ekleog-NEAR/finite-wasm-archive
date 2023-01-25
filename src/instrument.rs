use crate::{gas::InstrumentationKind, AnalysisOutcome};
use wasm_encoder::{self as we, Section};
use wasmparser as wp;

const PLACEHOLDER_FOR_NAMES: u8 = !0;
/// These function indices are known to be constant, as they are added at the beginning of the
/// imports section.
///
/// Doing so makes it much easier to transform references to other functions (basically add 2 to
/// all function indices)
const GAS_INSTRUMENTATION_FN: u32 = 0;

/// See [`GAS_INSTRUMENTATION_FN`].
const STACK_INSTRUMENTATION_FN: u32 = 1;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("could not parse an element")]
    ParseElement(#[source] wp::BinaryReaderError),
    #[error("could not parse an element item")]
    ParseElementItem(#[source] wp::BinaryReaderError),
    #[error("could not parse an element expression")]
    ParseElementExpression(#[source] wp::BinaryReaderError),
    #[error("could not parse the function locals")]
    ParseLocals(#[source] wp::BinaryReaderError),
    #[error("could not parse a function local")]
    ParseLocal(#[source] wp::BinaryReaderError),
    #[error("could not parse the function operators")]
    ParseOperators(#[source] wp::BinaryReaderError),
    #[error("could not parse an operator")]
    ParseOperator(#[source] wp::BinaryReaderError),
    #[error("could not parse an export")]
    ParseExport(#[source] wp::BinaryReaderError),
    #[error("could not parse a global")]
    ParseGlobal(#[source] wp::BinaryReaderError),
    #[error("could not parse a name section entry")]
    ParseName(#[source] wp::BinaryReaderError),
    #[error("could not parse a name map entry")]
    ParseNameMapName(#[source] wp::BinaryReaderError),
    #[error("could not parse an indirect name map entry")]
    ParseIndirectNameMapName(#[source] wp::BinaryReaderError),
    #[error("could not parse a module section header")]
    ParseModuleSection(#[source] wp::BinaryReaderError),
    #[error("could not parse a type section entry")]
    ParseType(#[source] wp::BinaryReaderError),
    #[error("could not parse an import section entry")]
    ParseImport(#[source] wp::BinaryReaderError),
    #[error("could not parse a function section entry")]
    ParseFunctionTyId(#[source] wp::BinaryReaderError),
    #[error("could not parse a constant expression operator")]
    ParseConstExprOperator(#[source] wp::BinaryReaderError),
    #[error("the analysis outcome missing a {0} entry for code section entry `{1}`")]
    FunctionMissingInAnalysisOutcome(&'static str, usize),
    #[error("{0} is too large to fit into i64 for code section entry `{1}`")]
    SizeTooLarge(&'static str, usize, #[source] std::num::TryFromIntError),
    #[error("module contains fewer function types than definitions")]
    InsufficientFunctionTypes,
    #[error("module contains a reference to an invalid type index")]
    InvalidTypeIndex,
}

pub(crate) struct InstrumentContext<'a> {
    analysis: &'a AnalysisOutcome,
    wasm: &'a [u8],
    import_env: &'a str,

    function_section: we::FunctionSection,
    type_section: we::TypeSection,
    import_section: we::ImportSection,
    code_section: we::CodeSection,
    element_section: we::ElementSection,
    export_section: we::ExportSection,
    name_section: we::NameSection,
    global_section: we::GlobalSection,
    start_section: we::StartSection,
    raw_sections: Vec<we::RawSection<'a>>,

    types: Vec<wp::Type>,
    function_types: std::vec::IntoIter<u32>,
}

impl<'a> InstrumentContext<'a> {
    pub(crate) fn new(wasm: &'a [u8], import_env: &'a str, analysis: &'a AnalysisOutcome) -> Self {
        Self {
            analysis,
            wasm,
            import_env,

            function_section: we::FunctionSection::new(),
            type_section: we::TypeSection::new(),
            import_section: we::ImportSection::new(),
            code_section: we::CodeSection::new(),
            element_section: we::ElementSection::new(),
            export_section: we::ExportSection::new(),
            name_section: we::NameSection::new(),
            global_section: we::GlobalSection::new(),
            start_section: we::StartSection { function_index: 0 },
            raw_sections: vec![],

            types: vec![],
            function_types: vec![].into_iter(),
        }
    }

    fn schedule_section(&mut self, id: u8) {
        self.raw_sections.push(we::RawSection { id, data: &[] });
    }

    pub(crate) fn run(mut self) -> Result<Vec<u8>, Error> {
        let parser = wp::Parser::new(0);
        for payload in parser.parse_all(self.wasm) {
            let payload = payload.map_err(Error::ParseModuleSection)?;
            match payload {
                // These two payload types are (re-)generated by wasm_encoder.
                wp::Payload::Version { .. } => {}
                wp::Payload::End(_) => {}
                // We must manually reconstruct the type section because we’re appending types to
                // it.
                wp::Payload::TypeSection(types) => {
                    for ty in types {
                        let ty = ty.map_err(Error::ParseType)?;
                        match &ty {
                            wp::Type::Func(f) => {
                                self.type_section.function(
                                    f.params().iter().copied().map(valtype),
                                    f.results().iter().copied().map(valtype),
                                );
                            }
                        }
                        self.types.push(ty);
                    }
                }
                // We must manually reconstruct the imports section because we’re appending imports
                // to it.
                wp::Payload::ImportSection(imports) => {
                    self.maybe_add_imports();
                    self.transform_imports_section(imports)?;
                }
                wp::Payload::StartSection { func, .. } => {
                    self.start_section.function_index = func + 2;
                    self.schedule_section(self.start_section.id());
                }
                wp::Payload::ElementSection(reader) => {
                    self.transform_elem_section(reader)?;
                    self.schedule_section(self.element_section.id());
                }
                wp::Payload::FunctionSection(reader) => {
                    // We don’t want to modify this, but need to remember function type indices…
                    let fn_types = reader
                        .into_iter()
                        .collect::<Result<Vec<u32>, _>>()
                        .map_err(Error::ParseFunctionTyId)?;
                    for fnty in &fn_types {
                        self.function_section.function(*fnty);
                    }
                    self.function_types = fn_types.into_iter();
                    self.schedule_section(self.function_section.id());
                }
                wp::Payload::CodeSectionStart { .. } => {
                    self.schedule_section(self.code_section.id());
                }
                wp::Payload::CodeSectionEntry(reader) => {
                    self.maybe_add_imports();
                    let type_index = self
                        .function_types
                        .next()
                        .ok_or(Error::InsufficientFunctionTypes)?;
                    self.transform_code_section(reader, type_index)?;
                }
                wp::Payload::ExportSection(reader) => {
                    for export in reader {
                        let export = export.map_err(Error::ParseExport)?;
                        let (kind, index) = match export.kind {
                            wp::ExternalKind::Func => (we::ExportKind::Func, export.index + 2),
                            wp::ExternalKind::Table => (we::ExportKind::Table, export.index),
                            wp::ExternalKind::Memory => (we::ExportKind::Memory, export.index),
                            wp::ExternalKind::Global => (we::ExportKind::Global, export.index),
                            wp::ExternalKind::Tag => (we::ExportKind::Tag, export.index),
                        };
                        self.export_section.export(export.name, kind, index);
                    }
                    self.schedule_section(self.export_section.id());
                }
                wp::Payload::GlobalSection(reader) => {
                    for global in reader {
                        let global = global.map_err(Error::ParseGlobal)?;
                        self.global_section.global(
                            we::GlobalType {
                                val_type: valtype(global.ty.content_type),
                                mutable: global.ty.mutable,
                            },
                            &constexpr(global.init_expr)?,
                        );
                    }
                    self.schedule_section(self.global_section.id());
                }
                wp::Payload::CustomSection(reader) if reader.name() == "name" => {
                    let names = wp::NameSectionReader::new(reader.data(), reader.data_offset());
                    self.transform_name_section(names)?;
                    self.schedule_section(PLACEHOLDER_FOR_NAMES)
                }
                // All the other sections are transparently copied over (they cannot reference a
                // function id, or we don’t know how to handle it anyhow)
                _ => {
                    let (id, range) = payload
                        .as_section()
                        .expect("any non-section payloads should have been handled already");
                    self.raw_sections.push(wasm_encoder::RawSection {
                        id,
                        data: &self.wasm[range],
                    });
                }
            }
        }

        // The type and import sections always come first in a module. They may potentially be
        // preceded or interspersed by custom sections in the original module, so we’re just hoping
        // that the ordering doesn’t matter for tests…
        let mut output = wasm_encoder::Module::new();
        output.section(&self.type_section);
        output.section(&self.import_section);
        for section in self.raw_sections {
            match section.id {
                id if id == self.code_section.id() => output.section(&self.code_section),
                id if id == self.element_section.id() => output.section(&self.element_section),
                id if id == self.export_section.id() => output.section(&self.export_section),
                id if id == self.global_section.id() => output.section(&self.global_section),
                id if id == self.start_section.id() => output.section(&self.start_section),
                id if id == self.function_section.id() => output.section(&self.function_section),
                PLACEHOLDER_FOR_NAMES => output.section(&self.name_section),
                _ => output.section(&section),
            };
        }
        Ok(output.finish())
    }

    fn transform_code_section(
        &mut self,
        reader: wp::FunctionBody,
        func_type_idx: u32,
    ) -> Result<(), Error> {
        let func_type_idx_usize =
            usize::try_from(func_type_idx).map_err(|_| Error::InvalidTypeIndex)?;
        let func_type = self
            .types
            .get(func_type_idx_usize)
            .ok_or(Error::InvalidTypeIndex)?;
        let locals = reader
            .get_locals_reader()
            .map_err(Error::ParseLocals)?
            .into_iter()
            .map(|v| v.map(|(c, t)| (c, valtype(t))))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::ParseLocal)?;
        let mut new_function = we::Function::new(locals);
        let code_idx = self.code_section.len() as usize;
        macro_rules! get_idx {
            (analysis . $field: ident) => {{
                let f = self.analysis.$field.get(code_idx);
                const NAME: &str = stringify!($field);
                f.ok_or(Error::FunctionMissingInAnalysisOutcome(NAME, code_idx))
            }};
        }
        let gas_costs = get_idx!(analysis.gas_costs)?;
        let gas_kinds = get_idx!(analysis.gas_kinds)?;
        let gas_offsets = get_idx!(analysis.gas_offsets)?;
        let stack_sz = *get_idx!(analysis.function_operand_stack_sizes)?;
        let frame_sz = *get_idx!(analysis.function_frame_sizes)?;
        let stack_sz = i64::try_from(stack_sz)
            .map_err(|e| Error::SizeTooLarge("function_operand_stack_sizes", code_idx, e))?;
        let frame_sz = i64::try_from(frame_sz)
            .map_err(|e| Error::SizeTooLarge("function_frame_sizes", code_idx, e))?;

        let mut instrumentation_points = gas_offsets
            .iter()
            .zip(gas_costs.iter())
            .zip(gas_kinds.iter())
            .peekable();
        let mut operators = reader
            .get_operators_reader()
            .map_err(Error::ParseOperators)?;

        // In order to enable us to insert the stack termination, we’ll wrap the function body into a
        // `block`…
        let (params, results) = match func_type {
            wp::Type::Func(fnty) => (fnty.params(), fnty.results()),
        };
        let block_type = match (params, results) {
            ([], []) => we::BlockType::Empty,
            (_, [result]) => we::BlockType::Result(valtype(*result)),
            ([], _) => we::BlockType::FunctionType(func_type_idx),
            (_, results) => {
                let new_block_type_idx = self.type_section.len();
                self.type_section
                    .function(std::iter::empty(), results.iter().copied().map(valtype));
                we::BlockType::FunctionType(new_block_type_idx)
            }
        };

        let should_instrument_stack = stack_sz != 0 || frame_sz != 0;
        if should_instrument_stack {
            new_function.instruction(&we::Instruction::Block(block_type));
            call_stack_instrumentation(&mut new_function, stack_sz, frame_sz);
        }

        while !operators.eof() {
            let (op, offset) = operators.read_with_offset().map_err(Error::ParseOperator)?;
            let end_offset = operators.original_position();
            if instrumentation_points.peek().map(|((o, _), _)| **o) == Some(offset) {
                let ((_, g), k) = instrumentation_points.next().expect("we just peeked");
                if !matches!(k, InstrumentationKind::Unreachable) {
                    call_gas_instrumentation(&mut new_function, *g)
                }
            }
            match op {
                wp::Operator::RefFunc { function_index } => {
                    new_function.instruction(&we::Instruction::RefFunc(function_index + 2))
                }
                wp::Operator::Call { function_index } => {
                    new_function.instruction(&we::Instruction::Call(function_index + 2))
                }
                wp::Operator::ReturnCall { function_index } => {
                    if should_instrument_stack {
                        call_stack_instrumentation(&mut new_function, -stack_sz, -frame_sz);
                    }
                    new_function.instruction(&we::Instruction::ReturnCall(function_index + 2))
                }
                wp::Operator::ReturnCallIndirect { .. } => {
                    if should_instrument_stack {
                        call_stack_instrumentation(&mut new_function, -stack_sz, -frame_sz);
                    }
                    new_function.raw(self.wasm[offset..end_offset].iter().copied())
                }
                wp::Operator::Return => {
                    if should_instrument_stack {
                        call_stack_instrumentation(&mut new_function, -stack_sz, -frame_sz);
                    }
                    new_function.instruction(&we::Instruction::Return)
                }
                wp::Operator::End if operators.eof() => {
                    // This is the last function end…
                    if should_instrument_stack {
                        new_function.instruction(&we::Instruction::End);
                        call_stack_instrumentation(&mut new_function, -stack_sz, -frame_sz);
                    }
                    new_function.instruction(&we::Instruction::End)
                }
                _ => new_function.raw(self.wasm[offset..end_offset].iter().copied()),
            };
        }

        self.code_section.function(&new_function);
        Ok(())
    }

    fn maybe_add_imports(&mut self) {
        if self.import_section.is_empty() {
            let instrument_fn_ty = self.type_section.len();
            // By adding the type at the end of the type section we guarantee that any other
            // type references remain valid.
            self.type_section.function([we::ValType::I64], []);
            self.type_section
                .function([we::ValType::I64, we::ValType::I64], []);
            // By inserting the imports at the beginning of the import section we make the new
            // function index mapping trivial (it is always just an increment by 2)
            self.import_section.import(
                self.import_env,
                "finite_wasm_gas",
                we::EntityType::Function(instrument_fn_ty),
            );
            self.import_section.import(
                self.import_env,
                "finite_wasm_stack",
                we::EntityType::Function(instrument_fn_ty + 1),
            );
        }
    }

    fn transform_name_section(&mut self, names: wp::NameSectionReader) -> Result<(), Error> {
        for name in names {
            let name = name.map_err(Error::ParseName)?;
            match name {
                wp::Name::Module { name, .. } => self.name_section.module(name),
                wp::Name::Function(map) => {
                    let mut new_name_map = namemap(map, true)?;
                    new_name_map.append(GAS_INSTRUMENTATION_FN, "finite_wasm_gas");
                    new_name_map.append(STACK_INSTRUMENTATION_FN, "finite_wasm_stack");
                    self.name_section.functions(&new_name_map)
                }
                wp::Name::Local(map) => self.name_section.locals(&indirectnamemap(map)?),
                wp::Name::Label(map) => self.name_section.labels(&indirectnamemap(map)?),
                wp::Name::Type(map) => self.name_section.types(&namemap(map, false)?),
                wp::Name::Table(map) => self.name_section.tables(&namemap(map, false)?),
                wp::Name::Memory(map) => self.name_section.memories(&namemap(map, false)?),
                wp::Name::Global(map) => self.name_section.globals(&namemap(map, false)?),
                wp::Name::Element(map) => self.name_section.elements(&namemap(map, false)?),
                wp::Name::Data(map) => self.name_section.data(&namemap(map, false)?),
                wp::Name::Unknown { .. } => {}
            }
        }
        Ok(())
    }

    fn transform_imports_section(&mut self, imports: wp::ImportSectionReader) -> Result<(), Error> {
        for import in imports {
            let import = import.map_err(Error::ParseImport)?;
            let import_ty = match import.ty {
                wp::TypeRef::Func(i) => we::EntityType::Function(i),
                wp::TypeRef::Table(t) => we::EntityType::Table(we::TableType {
                    element_type: valtype(t.element_type),
                    minimum: t.initial,
                    maximum: t.maximum,
                }),
                wp::TypeRef::Memory(t) => we::EntityType::Memory(we::MemoryType {
                    minimum: t.initial,
                    maximum: t.maximum,
                    memory64: t.memory64,
                    shared: t.shared,
                }),
                wp::TypeRef::Global(t) => we::EntityType::Global(we::GlobalType {
                    val_type: valtype(t.content_type),
                    mutable: t.mutable,
                }),
                wp::TypeRef::Tag(t) => we::EntityType::Tag(we::TagType {
                    kind: match t.kind {
                        wp::TagKind::Exception => we::TagKind::Exception,
                    },
                    func_type_idx: t.func_type_idx,
                }),
            };
            self.import_section
                .import(import.module, import.name, import_ty);
        }
        Ok(())
    }

    fn transform_elem_section(&mut self, reader: wp::ElementSectionReader) -> Result<(), Error> {
        for elem in reader {
            let elem = elem.map_err(Error::ParseElement)?;
            let functions;
            let expressions;
            let offset;
            let items = match elem.items {
                wp::ElementItems::Functions(fns) => {
                    functions = fns
                        .into_iter()
                        .map(|v| Ok(v.map_err(Error::ParseElementItem)? + 2))
                        .collect::<Result<Vec<_>, _>>()?;
                    we::Elements::Functions(&functions)
                }
                wp::ElementItems::Expressions(exprs) => {
                    expressions = exprs
                        .into_iter()
                        .map(|v| v.map_err(Error::ParseElementExpression).and_then(constexpr))
                        .collect::<Result<Vec<_>, _>>()?;
                    we::Elements::Expressions(&expressions)
                }
            };
            self.element_section.segment(we::ElementSegment {
                mode: match elem.kind {
                    wp::ElementKind::Passive => we::ElementMode::Passive,
                    wp::ElementKind::Declared => we::ElementMode::Declared,
                    wp::ElementKind::Active {
                        table_index,
                        offset_expr,
                    } => {
                        offset = constexpr(offset_expr)?;
                        we::ElementMode::Active {
                            table: Some(table_index),
                            offset: &offset,
                        }
                    }
                },
                element_type: valtype(elem.ty),
                elements: items,
            });
        }
        Ok(())
    }
}

fn call_stack_instrumentation(
    func: &mut we::Function,
    max_operand_stack_size: i64,
    function_frame_size: i64,
) {
    func.instruction(&we::Instruction::I64Const(max_operand_stack_size));
    func.instruction(&we::Instruction::I64Const(function_frame_size));
    // The interpreter prices this sequence as -4 gas to account for the leading `block` on
    // entry.
    //
    // It however does not exist on exit so introduce a dummy instruction to compensate.
    if max_operand_stack_size < 0 || function_frame_size < 0 {
        func.instruction(&we::Instruction::Nop);
    }
    func.instruction(&we::Instruction::Call(STACK_INSTRUMENTATION_FN));
}

fn call_gas_instrumentation(func: &mut we::Function, gas: u64) {
    if gas != 0 {
        // The reinterpreting cast is intentional here. On the other side the host function is
        // expected to reinterpret the argument back to u64.
        func.instruction(&we::Instruction::I64Const(gas as i64));
        func.instruction(&we::Instruction::Call(GAS_INSTRUMENTATION_FN));
    }
}

fn valtype(wp: wp::ValType) -> we::ValType {
    match wp {
        wp::ValType::I32 => we::ValType::I32,
        wp::ValType::I64 => we::ValType::I64,
        wp::ValType::F32 => we::ValType::F32,
        wp::ValType::F64 => we::ValType::F64,
        wp::ValType::V128 => we::ValType::V128,
        wp::ValType::FuncRef => we::ValType::FuncRef,
        wp::ValType::ExternRef => we::ValType::ExternRef,
    }
}

fn constexpr(ep: wp::ConstExpr) -> Result<we::ConstExpr, Error> {
    let mut reader = ep.get_binary_reader();
    Ok(
        match reader
            .clone()
            .read_operator()
            .map_err(Error::ParseConstExprOperator)?
        {
            wp::Operator::RefFunc { function_index } => we::ConstExpr::ref_func(function_index + 2),
            _ => {
                let expr_bytes = reader
                    .read_bytes(reader.bytes_remaining())
                    .expect("can't fail");
                // ConstExpr introduces its own `End` operand, so we want want to drop it.
                let without_end = &expr_bytes[0..expr_bytes.len() - 1];
                we::ConstExpr::raw(without_end.iter().copied())
            }
        },
    )
}

fn namemap(p: wp::NameMap, is_function: bool) -> Result<we::NameMap, Error> {
    let mut new_name_map = we::NameMap::new();
    for naming in p {
        let naming = naming.map_err(Error::ParseNameMapName)?;
        new_name_map.append(naming.index + (2u32 * is_function as u32), naming.name);
    }
    Ok(new_name_map)
}

fn indirectnamemap(p: wp::IndirectNameMap) -> Result<we::IndirectNameMap, Error> {
    let mut new_name_map = we::IndirectNameMap::new();
    for naming in p {
        let naming = naming.map_err(Error::ParseIndirectNameMapName)?;
        new_name_map.append(naming.index + 2, &namemap(naming.names, false)?);
    }
    Ok(new_name_map)
}