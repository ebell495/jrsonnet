//! jsonnet interpreter implementation

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(
	clippy::all,
	clippy::nursery,
	clippy::pedantic,
	// missing_docs,
	elided_lifetimes_in_paths,
	explicit_outlives_requirements,
	noop_method_call,
	single_use_lifetimes,
	variant_size_differences,
	rustdoc::all
)]
#![allow(
	macro_expanded_macro_exports_accessed_by_absolute_paths,
	clippy::ptr_arg,
	// Too verbose
	clippy::must_use_candidate,
	// A lot of functions pass around errors thrown by code
	clippy::missing_errors_doc,
	// A lot of pointers have interior Rc
	clippy::needless_pass_by_value,
	// Its fine
	clippy::wildcard_imports,
	clippy::enum_glob_use,
	clippy::module_name_repetitions,
	// TODO: fix individual issues, however this works as intended almost everywhere
	clippy::cast_precision_loss,
	clippy::cast_possible_wrap,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss,
	// False positives
	// https://github.com/rust-lang/rust-clippy/issues/6902
	clippy::use_self,
	// https://github.com/rust-lang/rust-clippy/issues/8539
	clippy::iter_with_drain,
)]

// For jrsonnet-macros
extern crate self as jrsonnet_evaluator;

mod ctx;
mod dynamic;
pub mod error;
mod evaluate;
pub mod function;
pub mod gc;
mod import;
mod integrations;
mod map;
mod obj;
pub mod stdlib;
pub mod trace;
pub mod typed;
pub mod val;

use std::{
	any::Any,
	cell::{Ref, RefCell, RefMut},
	collections::HashMap,
	fmt::{self, Debug},
	path::Path,
	rc::Rc,
};

pub use ctx::*;
pub use dynamic::*;
use error::{Error::*, LocError, Result, StackTraceElement};
pub use evaluate::*;
use function::{CallLocation, TlaArg};
use gc::{GcHashMap, TraceBox};
use hashbrown::hash_map::RawEntryMut;
pub use import::*;
use jrsonnet_gcmodule::{Cc, Trace};
pub use jrsonnet_interner::{IBytes, IStr};
pub use jrsonnet_parser as parser;
use jrsonnet_parser::*;
pub use obj::*;
use trace::{CompactFormat, TraceFormat};
pub use val::{ManifestFormat, Thunk, Val};

/// Thunk without bound `super`/`this`
/// object inheritance may be overriden multiple times, and will be fixed only on field read
pub trait Unbound: Trace {
	/// Type of value after object context is bound
	type Bound;
	/// Create value bound to specified object context
	fn bind(&self, s: State, sup: Option<ObjValue>, this: Option<ObjValue>) -> Result<Self::Bound>;
}

/// Object fields may, or may not depend on `this`/`super`, this enum allows cheaper reuse of object-independent fields for native code
/// Standard jsonnet fields are always unbound
#[derive(Clone, Trace)]
pub enum MaybeUnbound {
	/// Value needs to be bound to `this`/`super`
	Unbound(Cc<TraceBox<dyn Unbound<Bound = Thunk<Val>>>>),
	/// Value is object-independent
	Bound(Thunk<Val>),
}

impl Debug for MaybeUnbound {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "MaybeUnbound")
	}
}
impl MaybeUnbound {
	/// Attach object context to value, if required
	pub fn evaluate(
		&self,
		s: State,
		sup: Option<ObjValue>,
		this: Option<ObjValue>,
	) -> Result<Thunk<Val>> {
		match self {
			Self::Unbound(v) => v.bind(s, sup, this),
			Self::Bound(v) => Ok(v.clone()),
		}
	}
}

/// During import, this trait will be called to create initial context for file.
/// It may initialize global variables, stdlib for example.
pub trait ContextInitializer {
	/// Initialize default file context.
	fn initialize(&self, state: State, for_file: Source) -> Context;
	/// Allows upcasting from abstract to concrete context initializer.
	/// jrsonnet by itself doesn't use this method, it is allowed for it to panic.
	fn as_any(&self) -> &dyn Any;
}

/// Context initializer which adds nothing.
pub struct DummyContextInitializer;
impl ContextInitializer for DummyContextInitializer {
	fn initialize(&self, _state: State, _for_file: Source) -> Context {
		Context::default()
	}
	fn as_any(&self) -> &dyn Any {
		self
	}
}

/// Dynamically reconfigurable evaluation settings
pub struct EvaluationSettings {
	/// Limits recursion by limiting the number of stack frames
	pub max_stack: usize,
	/// Limits amount of stack trace items preserved
	pub max_trace: usize,
	/// TLA vars
	pub tla_vars: HashMap<IStr, TlaArg>,
	/// Context initializer, which will be used for imports and everything
	/// [`NoopContextInitializer`] is used by default, most likely you want to have `jrsonnet-stdlib`
	pub context_initializer: Box<dyn ContextInitializer>,
	/// Used to resolve file locations/contents
	pub import_resolver: Box<dyn ImportResolver>,
	/// Used in manifestification functions
	pub manifest_format: ManifestFormat,
	/// Used for bindings
	pub trace_format: Box<dyn TraceFormat>,
}
impl Default for EvaluationSettings {
	fn default() -> Self {
		Self {
			max_stack: 200,
			max_trace: 20,
			context_initializer: Box::new(DummyContextInitializer),
			tla_vars: HashMap::default(),
			import_resolver: Box::new(DummyImportResolver),
			manifest_format: ManifestFormat::Json {
				padding: 4,
				#[cfg(feature = "exp-preserve-order")]
				preserve_order: false,
			},
			trace_format: Box::new(CompactFormat {
				padding: 4,
				resolver: trace::PathResolver::Absolute,
			}),
		}
	}
}

#[derive(Default)]
struct EvaluationData {
	/// Used for stack overflow detection, stacktrace is populated on unwind
	stack_depth: usize,
	/// Updated every time stack entry is popt
	stack_generation: usize,

	breakpoints: Breakpoints,

	/// Contains file source codes and evaluation results for imports and pretty-printed stacktraces
	files: GcHashMap<SourcePath, FileData>,
}
struct FileData {
	string: Option<IStr>,
	bytes: Option<IBytes>,
	parsed: Option<LocExpr>,
	evaluated: Option<Val>,

	evaluating: bool,
}
impl FileData {
	fn new_string(data: IStr) -> Self {
		Self {
			string: Some(data),
			bytes: None,
			parsed: None,
			evaluated: None,
			evaluating: false,
		}
	}
	fn new_bytes(data: IBytes) -> Self {
		Self {
			string: None,
			bytes: Some(data),
			parsed: None,
			evaluated: None,
			evaluating: false,
		}
	}
}

#[allow(clippy::type_complexity)]
pub struct Breakpoint {
	loc: ExprLocation,
	collected: RefCell<HashMap<usize, (usize, Vec<Result<Val>>)>>,
}
#[derive(Default)]
struct Breakpoints(Vec<Rc<Breakpoint>>);
impl Breakpoints {
	fn insert(
		&self,
		stack_depth: usize,
		stack_generation: usize,
		loc: &ExprLocation,
		result: Result<Val>,
	) -> Result<Val> {
		if self.0.is_empty() {
			return result;
		}
		for item in &self.0 {
			if item.loc.belongs_to(loc) {
				let mut collected = item.collected.borrow_mut();
				let (depth, vals) = collected.entry(stack_generation).or_default();
				if stack_depth > *depth {
					vals.clear();
				}
				vals.push(result.clone());
			}
		}
		result
	}
}

#[derive(Default)]
pub struct EvaluationStateInternals {
	/// Internal state
	data: RefCell<EvaluationData>,
	/// Settings, safe to change at runtime
	settings: RefCell<EvaluationSettings>,
}

/// Maintains stack trace and import resolution
#[derive(Default, Clone)]
pub struct State(Rc<EvaluationStateInternals>);

impl State {
	/// Should only be called with path retrieved from [`resolve_path`], may panic otherwise
	pub fn import_resolved_str(&self, path: SourcePath) -> Result<IStr> {
		let mut data = self.data_mut();
		let mut file = data.files.raw_entry_mut().from_key(&path);

		let file = match file {
			RawEntryMut::Occupied(ref mut d) => d.get_mut(),
			RawEntryMut::Vacant(v) => {
				let data = self.settings().import_resolver.load_file_contents(&path)?;
				v.insert(
					path.clone(),
					FileData::new_string(
						std::str::from_utf8(&data)
							.map_err(|_| ImportBadFileUtf8(path.clone()))?
							.into(),
					),
				)
				.1
			}
		};
		if let Some(str) = &file.string {
			return Ok(str.clone());
		}
		if file.string.is_none() {
			file.string = Some(
				file.bytes
					.as_ref()
					.expect("either string or bytes should be set")
					.clone()
					.cast_str()
					.ok_or_else(|| ImportBadFileUtf8(path.clone()))?,
			);
		}
		Ok(file.string.as_ref().expect("just set").clone())
	}
	/// Should only be called with path retrieved from [`resolve_path`], may panic otherwise
	pub fn import_resolved_bin(&self, path: SourcePath) -> Result<IBytes> {
		let mut data = self.data_mut();
		let mut file = data.files.raw_entry_mut().from_key(&path);

		let file = match file {
			RawEntryMut::Occupied(ref mut d) => d.get_mut(),
			RawEntryMut::Vacant(v) => {
				let data = self.settings().import_resolver.load_file_contents(&path)?;
				v.insert(path.clone(), FileData::new_bytes(data.as_slice().into()))
					.1
			}
		};
		if let Some(str) = &file.bytes {
			return Ok(str.clone());
		}
		if file.bytes.is_none() {
			file.bytes = Some(
				file.string
					.as_ref()
					.expect("either string or bytes should be set")
					.clone()
					.cast_bytes(),
			);
		}
		Ok(file.bytes.as_ref().expect("just set").clone())
	}
	/// Should only be called with path retrieved from [`resolve_path`], may panic otherwise
	pub fn import_resolved(&self, path: SourcePath) -> Result<Val> {
		let mut data = self.data_mut();
		let mut file = data.files.raw_entry_mut().from_key(&path);

		let file = match file {
			RawEntryMut::Occupied(ref mut d) => d.get_mut(),
			RawEntryMut::Vacant(v) => {
				let data = self.settings().import_resolver.load_file_contents(&path)?;
				v.insert(
					path.clone(),
					FileData::new_string(
						std::str::from_utf8(&data)
							.map_err(|_| ImportBadFileUtf8(path.clone()))?
							.into(),
					),
				)
				.1
			}
		};
		if let Some(val) = &file.evaluated {
			return Ok(val.clone());
		}
		if file.string.is_none() {
			file.string = Some(
				std::str::from_utf8(
					file.bytes
						.as_ref()
						.expect("either string or bytes should be set"),
				)
				.map_err(|_| ImportBadFileUtf8(path.clone()))?
				.into(),
			);
		}
		let code = file.string.as_ref().expect("just set");
		let file_name = Source::new(path.clone(), code.clone());
		if file.parsed.is_none() {
			file.parsed = Some(
				jrsonnet_parser::parse(
					code,
					&ParserSettings {
						file_name: file_name.clone(),
					},
				)
				.map_err(|e| ImportSyntaxError {
					path: file_name.clone(),
					error: Box::new(e),
				})?,
			);
		}
		let parsed = file.parsed.as_ref().expect("just set").clone();
		if file.evaluating {
			throw!(InfiniteRecursionDetected)
		}
		file.evaluating = true;
		// Dropping file here, as it borrows data, which may be used in evaluation
		drop(data);
		let res = evaluate(
			self.clone(),
			self.create_default_context(file_name),
			&parsed,
		);

		let mut data = self.data_mut();
		let mut file = data.files.raw_entry_mut().from_key(&path);

		let file = match file {
			RawEntryMut::Occupied(ref mut d) => d.get_mut(),
			RawEntryMut::Vacant(_) => unreachable!("this file was just here!"),
		};
		file.evaluating = false;
		match res {
			Ok(v) => {
				file.evaluated = Some(v.clone());
				Ok(v)
			}
			Err(e) => Err(e),
		}
	}

	/// Has same semantics as `import 'path'` called from `from` file
	pub fn import_from(&self, from: &SourcePath, path: &str) -> Result<Val> {
		let resolved = self.resolve_from(from, path)?;
		self.import_resolved(resolved)
	}
	pub fn import(&self, path: impl AsRef<Path>) -> Result<Val> {
		let resolved = self.resolve(path)?;
		self.import_resolved(resolved)
	}

	/// Creates context with all passed global variables
	pub fn create_default_context(&self, source: Source) -> Context {
		let context_initializer = &self.settings().context_initializer;
		context_initializer.initialize(self.clone(), source)
	}

	/// Executes code creating a new stack frame
	pub fn push<T>(
		&self,
		e: CallLocation<'_>,
		frame_desc: impl FnOnce() -> String,
		f: impl FnOnce() -> Result<T>,
	) -> Result<T> {
		{
			let mut data = self.data_mut();
			let stack_depth = &mut data.stack_depth;
			if *stack_depth > self.max_stack() {
				// Error creation uses data, so i drop guard here
				drop(data);
				throw!(StackOverflow);
			}
			*stack_depth += 1;
		}
		let result = f();
		{
			let mut data = self.data_mut();
			data.stack_depth -= 1;
			data.stack_generation += 1;
		}
		if let Err(mut err) = result {
			err.trace_mut().0.push(StackTraceElement {
				location: e.0.cloned(),
				desc: frame_desc(),
			});
			return Err(err);
		}
		result
	}

	/// Executes code creating a new stack frame
	pub fn push_val(
		&self,
		e: &ExprLocation,
		frame_desc: impl FnOnce() -> String,
		f: impl FnOnce() -> Result<Val>,
	) -> Result<Val> {
		{
			let mut data = self.data_mut();
			let stack_depth = &mut data.stack_depth;
			if *stack_depth > self.max_stack() {
				// Error creation uses data, so i drop guard here
				drop(data);
				throw!(StackOverflow);
			}
			*stack_depth += 1;
		}
		let mut result = f();
		{
			let mut data = self.data_mut();
			data.stack_depth -= 1;
			data.stack_generation += 1;
			result = data
				.breakpoints
				.insert(data.stack_depth, data.stack_generation, e, result);
		}
		if let Err(mut err) = result {
			err.trace_mut().0.push(StackTraceElement {
				location: Some(e.clone()),
				desc: frame_desc(),
			});
			return Err(err);
		}
		result
	}
	/// Executes code creating a new stack frame
	pub fn push_description<T>(
		&self,
		frame_desc: impl FnOnce() -> String,
		f: impl FnOnce() -> Result<T>,
	) -> Result<T> {
		{
			let mut data = self.data_mut();
			let stack_depth = &mut data.stack_depth;
			if *stack_depth > self.max_stack() {
				// Error creation uses data, so i drop guard here
				drop(data);
				throw!(StackOverflow);
			}
			*stack_depth += 1;
		}
		let result = f();
		{
			let mut data = self.data_mut();
			data.stack_depth -= 1;
			data.stack_generation += 1;
		}
		if let Err(mut err) = result {
			err.trace_mut().0.push(StackTraceElement {
				location: None,
				desc: frame_desc(),
			});
			return Err(err);
		}
		result
	}

	/// # Panics
	/// In case of formatting failure
	pub fn stringify_err(&self, e: &LocError) -> String {
		let mut out = String::new();
		self.settings()
			.trace_format
			.write_trace(&mut out, self, e)
			.unwrap();
		out
	}

	pub fn manifest(&self, val: Val) -> Result<IStr> {
		self.push_description(
			|| "manifestification".to_string(),
			|| val.manifest(self.clone(), &self.manifest_format()),
		)
	}
	pub fn manifest_multi(&self, val: Val) -> Result<Vec<(IStr, IStr)>> {
		val.manifest_multi(self.clone(), &self.manifest_format())
	}
	pub fn manifest_stream(&self, val: Val) -> Result<Vec<IStr>> {
		val.manifest_stream(self.clone(), &self.manifest_format())
	}

	/// If passed value is function then call with set TLA
	pub fn with_tla(&self, val: Val) -> Result<Val> {
		Ok(match val {
			Val::Func(func) => self.push_description(
				|| "during TLA call".to_owned(),
				|| {
					func.evaluate(
						self.clone(),
						self.create_default_context(Source::new_virtual(
							"<tla>".into(),
							IStr::empty(),
						)),
						CallLocation::native(),
						&self.settings().tla_vars,
						true,
					)
				},
			)?,
			v => v,
		})
	}
}

/// Internals
impl State {
	fn data_mut(&self) -> RefMut<'_, EvaluationData> {
		self.0.data.borrow_mut()
	}
	pub fn settings(&self) -> Ref<'_, EvaluationSettings> {
		self.0.settings.borrow()
	}
	pub fn settings_mut(&self) -> RefMut<'_, EvaluationSettings> {
		self.0.settings.borrow_mut()
	}
}

/// Raw methods evaluate passed values but don't perform TLA execution
impl State {
	/// Parses and evaluates the given snippet
	pub fn evaluate_snippet(&self, name: impl Into<IStr>, code: impl Into<IStr>) -> Result<Val> {
		let code = code.into();
		let source = Source::new_virtual(name.into(), code.clone());
		let parsed = jrsonnet_parser::parse(
			&code,
			&ParserSettings {
				file_name: source.clone(),
			},
		)
		.map_err(|e| ImportSyntaxError {
			path: source.clone(),
			error: Box::new(e),
		})?;
		evaluate(self.clone(), self.create_default_context(source), &parsed)
	}
}

/// Settings utilities
impl State {
	pub fn add_tla(&self, name: IStr, value: Val) {
		self.settings_mut()
			.tla_vars
			.insert(name, TlaArg::Val(value));
	}
	pub fn add_tla_str(&self, name: IStr, value: IStr) {
		self.settings_mut()
			.tla_vars
			.insert(name, TlaArg::String(value));
	}
	pub fn add_tla_code(&self, name: IStr, code: &str) -> Result<()> {
		let source_name = format!("<top-level-arg:{name}>");
		let source = Source::new_virtual(source_name.into(), code.into());
		let parsed = jrsonnet_parser::parse(
			code,
			&ParserSettings {
				file_name: source.clone(),
			},
		)
		.map_err(|e| ImportSyntaxError {
			path: source,
			error: Box::new(e),
		})?;
		self.settings_mut()
			.tla_vars
			.insert(name, TlaArg::Code(parsed));
		Ok(())
	}

	// Only panics in case of [`ImportResolver`] contract violation
	#[allow(clippy::missing_panics_doc)]
	pub fn resolve_from(&self, from: &SourcePath, path: &str) -> Result<SourcePath> {
		self.import_resolver().resolve_from(from, path.as_ref())
	}

	// Only panics in case of [`ImportResolver`] contract violation
	#[allow(clippy::missing_panics_doc)]
	pub fn resolve(&self, path: impl AsRef<Path>) -> Result<SourcePath> {
		self.import_resolver().resolve(path.as_ref())
	}
	pub fn import_resolver(&self) -> Ref<'_, dyn ImportResolver> {
		Ref::map(self.settings(), |s| &*s.import_resolver)
	}
	pub fn set_import_resolver(&self, resolver: Box<dyn ImportResolver>) {
		self.settings_mut().import_resolver = resolver;
	}
	pub fn context_initializer(&self) -> Ref<'_, dyn ContextInitializer> {
		Ref::map(self.settings(), |s| &*s.context_initializer)
	}

	pub fn manifest_format(&self) -> ManifestFormat {
		self.settings().manifest_format.clone()
	}
	pub fn set_manifest_format(&self, format: ManifestFormat) {
		self.settings_mut().manifest_format = format;
	}

	pub fn trace_format(&self) -> Ref<'_, dyn TraceFormat> {
		Ref::map(self.settings(), |s| &*s.trace_format)
	}
	pub fn set_trace_format(&self, format: Box<dyn TraceFormat>) {
		self.settings_mut().trace_format = format;
	}

	pub fn max_trace(&self) -> usize {
		self.settings().max_trace
	}
	pub fn set_max_trace(&self, trace: usize) {
		self.settings_mut().max_trace = trace;
	}

	pub fn max_stack(&self) -> usize {
		self.settings().max_stack
	}
	pub fn set_max_stack(&self, trace: usize) {
		self.settings_mut().max_stack = trace;
	}
}
