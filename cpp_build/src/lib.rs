//! This crate is the `cpp` cargo build script implementation. It is useless
//! without the companion crates `cpp`, and `cpp_macro`.
//!
//! For more information, see the
//! [`cpp` crate module level documentation](https://docs.rs/cpp).

#![allow(clippy::write_with_newline)]

mod strnom;

use cpp_common::*;
use lazy_static::lazy_static;
use std::collections::hash_map::{Entry, HashMap};
use std::env;
use std::fs::{create_dir, remove_dir_all, File};
use std::io::prelude::*;
use std::path::{Path, PathBuf};

mod parser;

fn warnln_impl(a: &str) {
    for s in a.lines() {
        println!("cargo:warning={}", s);
    }
}

macro_rules! warnln {
    ($($all:tt)*) => {
        $crate::warnln_impl(&format!($($all)*));
    }
}

// Like the write! macro, but add the #line directive (pointing to this file).
// Note: the string literal must be on on the same line of the macro
macro_rules! write_add_line {
    ($o:expr, $($e:tt)*) => {
        (|| {
            writeln!($o, "#line {} \"{}\"", line!(), file!().replace('\\', "\\\\"))?;
            write!($o, $($e)*)
        })()
    };
}

const INTERNAL_CPP_STRUCTS: &str = r#"
/* THIS FILE IS GENERATED BY rust-cpp. DO NOT EDIT */

#include "stdint.h" // For {u}intN_t
#include <new> // For placement new
#include <cstdlib> // For abort
#include <type_traits>
#include <utility>

namespace rustcpp {

// We can't just pass or return any type from extern "C" rust functions (because the call
// convention may differ between the C++ type, and the Rust type).
// So we make sure to pass trivial structure that only contains a pointer to the object we want to
// pass. The constructor of these helper class contains a 'container' of the right size which will
// be allocated on the stack.
template<typename T> struct return_helper {
    struct container {
#if defined (_MSC_VER) && (_MSC_VER + 0 < 1900)
        char memory[sizeof(T)];
        ~container() { reinterpret_cast<T*>(this)->~T(); }
#else
        // The fact that it is in an union means it is properly sized and aligned, but we have
        // to call the destructor and constructor manually
        union { T memory; };
        ~container() { memory.~T(); }
#endif
        container() {}
    };
    const container* data;
    return_helper(int, const container &c = container()) : data(&c) { }
};

template<typename T> struct argument_helper {
    using type = const T&;
};
template<typename T> struct argument_helper<T&> {
    T &ref;
    argument_helper(T &x) : ref(x) {}
    using type = argument_helper<T&> const&;
};

template<typename T>
typename std::enable_if<std::is_copy_constructible<T>::value>::type copy_helper(const void *src, void *dest)
{ new (dest) T (*static_cast<T const*>(src)); }
template<typename T>
typename std::enable_if<!std::is_copy_constructible<T>::value>::type copy_helper(const void *, void *)
{ std::abort(); }
template<typename T>
typename std::enable_if<std::is_default_constructible<T>::value>::type default_helper(void *dest)
{ new (dest) T(); }
template<typename T>
typename std::enable_if<!std::is_default_constructible<T>::value>::type default_helper(void *)
{ std::abort(); }

template<typename T> int compare_helper(const T &a, const T&b, int cmp) {
    switch (cmp) {
        using namespace std::rel_ops;
        case 0:
            if (a < b)
                return -1;
            if (b < a)
                return 1;
            return 0;
        case -2: return a < b;
        case 2: return a > b;
        case -1: return a <= b;
        case 1: return a >= b;
    }
    std::abort();
}
}

#define RUST_CPP_CLASS_HELPER(HASH, ...) \
    extern "C" { \
    void __cpp_destructor_##HASH(void *ptr) { typedef __VA_ARGS__ T; static_cast<T*>(ptr)->~T(); } \
    void __cpp_copy_##HASH(const void *src, void *dest) { rustcpp::copy_helper<__VA_ARGS__>(src, dest); } \
    void __cpp_default_##HASH(void *dest) { rustcpp::default_helper<__VA_ARGS__>(dest); } \
    }
"#;

lazy_static! {
    static ref CPP_DIR: PathBuf = OUT_DIR.join("rust_cpp");
    static ref CARGO_MANIFEST_DIR: PathBuf = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect(
        r#"
-- rust-cpp fatal error --

The CARGO_MANIFEST_DIR environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#
    ));
}

fn gen_cpp_lib(visitor: &parser::Parser) -> PathBuf {
    let result_path = CPP_DIR.join("cpp_closures.cpp");
    let mut output = File::create(&result_path).expect("Unable to generate temporary C++ file");

    write!(output, "{}", INTERNAL_CPP_STRUCTS).unwrap();

    if visitor.callbacks_count > 0 {
        #[rustfmt::skip]
        write_add_line!(output, r#"
extern "C" {{
    void (*rust_cpp_callbacks{file_hash}[{callbacks_count}])() = {{}};
}}
        "#,
            file_hash = *FILE_HASH,
            callbacks_count = visitor.callbacks_count
        ).unwrap();
    }

    write!(output, "{}\n\n", &visitor.snippets).unwrap();

    let mut hashmap = HashMap::new();

    let mut sizealign = vec![];
    for Closure { body_str, sig, callback_offset, .. } in &visitor.closures {
        let ClosureSig { captures, cpp, .. } = sig;

        let hash = sig.name_hash();
        let name = sig.extern_name();

        match hashmap.entry(hash) {
            Entry::Occupied(e) => {
                if *e.get() != sig {
                    // Let the compiler do a compilation error. FIXME: report a better error
                    warnln!("Hash collision detected.");
                } else {
                    continue;
                }
            }
            Entry::Vacant(e) => {
                e.insert(sig);
            }
        }

        let is_void = cpp == "void";

        // Generate the sizes array with the sizes of each of the argument types
        if is_void {
            sizealign.push(format!(
                "{{{hash}ull, 0, 1, {callback_offset}ull << 32}}",
                hash = hash,
                callback_offset = callback_offset
            ));
        } else {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value | {callback_offset}ull << 32
            }}", hash=hash, type=cpp, callback_offset = callback_offset));
        }
        for Capture { cpp, .. } in captures {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=cpp));
        }

        // Generate the parameters and function declaration
        let params = captures
            .iter()
            .map(|&Capture { mutable, ref name, ref cpp }| {
                if mutable {
                    format!("{} & {}", cpp, name)
                } else {
                    format!("{} const& {}", cpp, name)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        if is_void {
            #[rustfmt::skip]
            write_add_line!(output, r#"
extern "C" {{
void {name}({params}) {{
{body}
}}
}}
"#,
                name = &name,
                params = params,
                body = body_str
            ).unwrap();
        } else {
            let comma = if params.is_empty() { "" } else { "," };
            let args = captures
                .iter()
                .map(|Capture { name, .. }| name.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            #[rustfmt::skip]
            write_add_line!(output, r#"
static inline {ty} {name}_impl({params}) {{
{body}
}}
extern "C" {{
void {name}({params}{comma} void* __result) {{
    ::new(__result) ({ty})({name}_impl({args}));
}}
}}
"#,
                name = &name,
                params = params,
                comma = comma,
                ty = cpp,
                args = args,
                body = body_str
            ).unwrap();
        }
    }

    for class in &visitor.classes {
        let hash = class.name_hash();

        // Generate the sizes array
        sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=class.cpp));

        // Generate helper function.
        // (this is done in a macro, which right after a #line directing pointing to the location of
        // the cpp_class! macro in order to give right line information in the possible errors)
        write!(
            output,
            "{line}RUST_CPP_CLASS_HELPER({hash}, {cpp_name})\n",
            line = class.line,
            hash = hash,
            cpp_name = class.cpp
        )
        .unwrap();

        if class.derives("PartialEq") {
            write!(output,
                "{line}extern \"C\" bool __cpp_equal_{hash}(const {name} *a, const {name} *b) {{ return *a == *b; }}\n",
                line = class.line, hash = hash, name = class.cpp).unwrap();
        }
        if class.derives("PartialOrd") {
            write!(output,
                "{line}extern \"C\" bool __cpp_compare_{hash}(const {name} *a, const {name} *b, int cmp) {{ return rustcpp::compare_helper(*a, *b, cmp); }}\n",
                line = class.line, hash = hash, name = class.cpp).unwrap();
        }
    }

    let mut magic = vec![];
    for mag in STRUCT_METADATA_MAGIC.iter() {
        magic.push(format!("{}", mag));
    }

    #[rustfmt::skip]
    write_add_line!(output, r#"

namespace rustcpp {{

template<typename T>
struct AlignOf {{
    struct Inner {{
        char a;
        T b;
    }};
    static const uintptr_t value = sizeof(Inner) - sizeof(T);
}};

template<typename T>
struct Flags {{
    static const uintptr_t value =
        (std::is_copy_constructible<T>::value << {flag_is_copy_constructible}) |
        (std::is_default_constructible<T>::value << {flag_is_default_constructible}) |
#if !defined(__GNUC__) || (__GNUC__ + 0 >= 5) || defined(__clang__)
        (std::is_trivially_destructible<T>::value << {flag_is_trivially_destructible}) |
        (std::is_trivially_copyable<T>::value << {flag_is_trivially_copyable}) |
        (std::is_trivially_default_constructible<T>::value << {flag_is_trivially_default_constructible}) |
#endif
        0;
}};

struct SizeAlign {{
    uint64_t hash;
    uint64_t size;
    uint64_t align;
    uint64_t flags;
}};

struct MetaData {{
    uint8_t magic[128];
    uint8_t version[16];
    uint64_t endianness_check;
    uint64_t length;
    SizeAlign data[{length}];
}};

MetaData metadata_{hash} = {{
    {{ {magic} }},
    "{version}",
    0xffef,
    {length},
    {{ {data} }}
}};

}} // namespace rustcpp
"#,
        hash = *FILE_HASH,
        data = sizealign.join(", "),
        length = sizealign.len(),
        magic = magic.join(", "),
        version = VERSION,
        flag_is_copy_constructible = flags::IS_COPY_CONSTRUCTIBLE,
        flag_is_default_constructible = flags::IS_DEFAULT_CONSTRUCTIBLE,
        flag_is_trivially_destructible = flags::IS_TRIVIALLY_DESTRUCTIBLE,
        flag_is_trivially_copyable = flags::IS_TRIVIALLY_COPYABLE,
        flag_is_trivially_default_constructible = flags::IS_TRIVIALLY_DEFAULT_CONSTRUCTIBLE,
    ).unwrap();

    result_path
}

fn clean_artifacts() {
    if CPP_DIR.is_dir() {
        remove_dir_all(&*CPP_DIR).expect(
            r#"
-- rust-cpp fatal error --

Failed to remove existing build artifacts from output directory."#,
        );
    }

    create_dir(&*CPP_DIR).expect(
        r#"
-- rust-cpp fatal error --

Failed to create output object directory."#,
    );
}

/// This struct is for advanced users of the build script. It allows providing
/// configuration options to `cpp` and the compiler when it is used to build.
///
/// ## API Note
///
/// Internally, `cpp` uses the `cc` crate to build the compilation artifact,
/// and many of the methods defined on this type directly proxy to an internal
/// `cc::Build` object.
pub struct Config {
    cc: cc::Build,
    std_flag_set: bool, // true if the -std flag was specified
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

/// Convert from a preconfigured [`cc::Build`] object to [`Config`].
/// Note that this will ensure C++ is enabled and add the proper include
/// directories to work with `cpp`.
impl From<cc::Build> for Config {
    fn from(mut cc: cc::Build) -> Self {
        cc.cpp(true).include(&*CARGO_MANIFEST_DIR);
        Self { cc, std_flag_set: false }
    }
}

impl Config {
    /// Create a new `Config` object. This object will hold the configuration
    /// options which control the build. If you don't need to make any changes,
    /// `cpp_build::build` is a wrapper function around this interface.
    pub fn new() -> Config {
        cc::Build::new().into()
    }

    /// Add a directory to the `-I` or include path for headers
    pub fn include<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cc.include(dir);
        self
    }

    /// Specify a `-D` variable with an optional value
    pub fn define(&mut self, var: &str, val: Option<&str>) -> &mut Self {
        self.cc.define(var, val);
        self
    }

    // XXX: Make sure that this works with sizes logic
    /// Add an arbitrary object file to link in
    pub fn object<P: AsRef<Path>>(&mut self, obj: P) -> &mut Self {
        self.cc.object(obj);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler
    pub fn flag(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag(flag);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler if it supports it
    pub fn flag_if_supported(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag_if_supported(flag);
        self
    }

    // XXX: Make sure this works with sizes logic
    /// Add a file which will be compiled
    pub fn file<P: AsRef<Path>>(&mut self, p: P) -> &mut Self {
        self.cc.file(p);
        self
    }

    /// Set the standard library to link against when compiling with C++
    /// support.
    ///
    /// The default value of this property depends on the current target: On
    /// OS X `Some("c++")` is used, when compiling for a Visual Studio based
    /// target `None` is used and for other targets `Some("stdc++")` is used.
    ///
    /// A value of `None` indicates that no automatic linking should happen,
    /// otherwise cargo will link against the specified library.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_link_stdlib(&mut self, cpp_link_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_link_stdlib(cpp_link_stdlib);
        self
    }

    /// Force the C++ compiler to use the specified standard library.
    ///
    /// Setting this option will automatically set `cpp_link_stdlib` to the same
    /// value.
    ///
    /// The default value of this option is always `None`.
    ///
    /// This option has no effect when compiling for a Visual Studio based
    /// target.
    ///
    /// This option sets the `-stdlib` flag, which is only supported by some
    /// compilers (clang, icc) but not by others (gcc). The library will not
    /// detect which compiler is used, as such it is the responsibility of the
    /// caller to ensure that this option is only used in conjuction with a
    /// compiler which supports the `-stdlib` flag.
    ///
    /// A value of `None` indicates that no specific C++ standard library should
    /// be used, otherwise `-stdlib` is added to the compile invocation.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_set_stdlib(&mut self, cpp_set_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_set_stdlib(cpp_set_stdlib);
        self
    }

    // XXX: Add support for custom targets
    //
    // /// Configures the target this configuration will be compiling for.
    // ///
    // /// This option is automatically scraped from the `TARGET` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn target(&mut self, target: &str) -> &mut Self {
    //     self.cc.target(target);
    //     self
    // }

    /// Configures the host assumed by this configuration.
    ///
    /// This option is automatically scraped from the `HOST` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn host(&mut self, host: &str) -> &mut Self {
        self.cc.host(host);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level(&mut self, opt_level: u32) -> &mut Self {
        self.cc.opt_level(opt_level);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level_str(&mut self, opt_level: &str) -> &mut Self {
        self.cc.opt_level_str(opt_level);
        self
    }

    /// Configures whether the compiler will emit debug information when
    /// generating object files.
    ///
    /// This option is automatically scraped from the `PROFILE` environment
    /// variable by build scripts (only enabled when the profile is "debug"), so
    /// it's not required to call this function.
    pub fn debug(&mut self, debug: bool) -> &mut Self {
        self.cc.debug(debug);
        self
    }

    // XXX: Add support for custom out_dir
    //
    // /// Configures the output directory where all object files and static
    // /// libraries will be located.
    // ///
    // /// This option is automatically scraped from the `OUT_DIR` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn out_dir<P: AsRef<Path>>(&mut self, out_dir: P) -> &mut Self {
    //     self.cc.out_dir(out_dir);
    //     self
    // }

    /// Configures the compiler to be used to produce output.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn compiler<P: AsRef<Path>>(&mut self, compiler: P) -> &mut Self {
        self.cc.compiler(compiler);
        self
    }

    /// Configures the tool used to assemble archives.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn archiver<P: AsRef<Path>>(&mut self, archiver: P) -> &mut Self {
        self.cc.archiver(archiver);
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Self {
        // XXX: Use this to control the cargo metadata which rust-cpp produces
        self.cc.cargo_metadata(cargo_metadata);
        self
    }

    /// Configures whether the compiler will emit position independent code.
    ///
    /// This option defaults to `false` for `i686` and `windows-gnu` targets and
    /// to `true` for all other targets.
    pub fn pic(&mut self, pic: bool) -> &mut Self {
        self.cc.pic(pic);
        self
    }

    /// Extracts `cpp` declarations from the passed-in crate root, and builds
    /// the associated static library to be linked in to the final binary.
    ///
    /// This method does not perform rust codegen - that is performed by `cpp`
    /// and `cpp_macros`, which perform the actual procedural macro expansion.
    ///
    /// This method may technically be called more than once for ergonomic
    /// reasons, but that usually won't do what you want. Use a different
    /// `Config` object each time you want to build a crate.
    pub fn build<P: AsRef<Path>>(&mut self, crate_root: P) {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            VERSION,
            "Internal Error: mismatched cpp_common and cpp_build versions"
        );

        // Clean up any leftover artifacts
        clean_artifacts();

        // Parse the crate
        let mut visitor = parser::Parser::default();
        if let Err(err) = visitor.parse_crate(crate_root.as_ref().to_owned()) {
            warnln!(
                r#"-- rust-cpp parse error --
There was an error parsing the crate for the rust-cpp build script:
{}
In order to provide a better error message, the build script will exit successfully, such that rustc can provide an error message."#,
                err
            );
            return;
        }

        // Generate the C++ library code
        let filename = gen_cpp_lib(&visitor);

        // Ensure C++11 mode is enabled. We rely on some C++11 construct, so we
        // must enable C++11 by default.
        // MSVC, GCC >= 5, Clang >= 6 defaults to C++14, but since we want to
        // supports older compiler which defaults to C++98, we need to
        // explicitly set the "-std" flag.
        // Ideally should be done by https://github.com/alexcrichton/cc-rs/issues/191
        if !self.std_flag_set {
            self.cc.flag_if_supported("-std=c++11");
        }
        // Build the C++ library
        if let Err(e) = self.cc.file(filename).try_compile(LIB_NAME) {
            let _ = writeln!(std::io::stderr(), "\n\nerror occurred: {}\n\n", e);
            #[cfg(not(feature = "docs-only"))]
            std::process::exit(1);
        }
    }
}

/// Run the `cpp` build process on the crate with a root at the given path.
/// Intended to be used within `build.rs` files.
pub fn build<P: AsRef<Path>>(path: P) {
    Config::new().build(path)
}
