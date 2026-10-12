//! Build script for `agalma-ipc`: generate the ConnectRPC envelope types.
//!
//! `connectrpc-build` normally obtains a `FileDescriptorSet` by shelling out to
//! `protoc` (or `buf`). Neither binary exists in this workspace's offline build,
//! and the dependency policy permits no extra downloaded binaries, so this
//! script reads `proto/ipc.proto` with a small proto3 reader and hands
//! `connectrpc-build` a descriptor set built in-process. `ipc.proto` stays the
//! source of truth; the reader raises on anything it does not understand rather
//! than silently mis-compiling.
//!
//! The generated module tree is included at `crate::proto` via
//! [`connectrpc::include_generated!`].

use std::path::PathBuf;

use buffa::Message as _;
use buffa_codegen::generated::descriptor::field_descriptor_proto::{Label, Type};
use buffa_codegen::generated::descriptor::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    MethodDescriptorProto, OneofDescriptorProto, ServiceDescriptorProto,
};

fn main() {
    println!("cargo:rerun-if-changed=proto/ipc.proto");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let proto_path = manifest_dir.join("proto/ipc.proto");
    let src = std::fs::read_to_string(&proto_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", proto_path.display()));

    let schema = parse_proto(&src);
    let fds = schema.to_descriptor();

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let fds_path = out_dir.join("ipc.fds");
    std::fs::write(&fds_path, fds.encode_to_vec()).expect("write descriptor set");

    connectrpc_build::Config::new()
        .files(&["ipc.proto"])
        .descriptor_set(&fds_path)
        .include_file("_connectrpc.rs")
        // Proto-only build: Agalma's JSON rides in `bytes`, so the proto3-JSON
        // codec (serde derives + `buffa::json_helpers`) is off, matching
        // `connectrpc`'s `json` feature being disabled (design D2).
        .generate_json(false)
        .emit_register_fn(false)
        .compile()
        .expect("connectrpc-build codegen");
}

// ---------------------------------------------------------------------------
// Minimal proto3 reader (messages, fields, services)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Tok {
    Ident(String),
    Num(i64),
    Str(String),
    Sym(char),
}

fn tokenize(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == '"' {
            i += 1;
            let start = i;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            toks.push(Tok::Str(src[start..i].to_string()));
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_alphanumeric() || b.get(i) == Some(&b'_') {
                i += 1;
            }
            toks.push(Tok::Ident(src[start..i].to_string()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
            toks.push(Tok::Num(src[start..i].parse().expect("field number")));
        } else {
            toks.push(Tok::Sym(c));
            i += 1;
        }
    }
    toks
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bang(&mut self, what: &str) -> ! {
        panic!("proto reader: expected {what} at token {}", self.pos);
    }

    fn expect_sym(&mut self, sym: char) {
        match self.toks.get(self.pos) {
            Some(Tok::Sym(c)) if *c == sym => self.pos += 1,
            _ => self.bang(&format!("`{sym}`")),
        }
    }

    fn expect_ident(&mut self) -> String {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Ident(s)) => {
                self.pos += 1;
                s
            }
            _ => self.bang("identifier"),
        }
    }

    fn expect_num(&mut self) -> i64 {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                n
            }
            _ => self.bang("number"),
        }
    }

    fn expect_str(&mut self) -> String {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                s
            }
            _ => self.bang("string"),
        }
    }

    /// `a.b.c` as a dotted name.
    fn dotted_name(&mut self) -> String {
        let mut name = self.expect_ident();
        while matches!(self.peek(), Some(Tok::Sym('.'))) {
            self.expect_sym('.');
            name.push('.');
            name.push_str(&self.expect_ident());
        }
        name
    }
}

#[derive(Debug)]
struct FieldDef {
    label: Label,
    optional_kw: bool,
    type_name: String,
    name: String,
    number: i32,
}

#[derive(Debug)]
struct MessageDef {
    name: String,
    fields: Vec<FieldDef>,
}

#[derive(Debug)]
struct MethodDef {
    name: String,
    input: String,
    output: String,
}

#[derive(Debug)]
struct ServiceDef {
    name: String,
    methods: Vec<MethodDef>,
}

#[derive(Debug, Default)]
struct Schema {
    package: String,
    messages: Vec<MessageDef>,
    services: Vec<ServiceDef>,
}

fn parse_proto(src: &str) -> Schema {
    let toks = tokenize(src);
    let mut p = Parser { toks, pos: 0 };
    let mut schema = Schema::default();

    while let Some(tok) = p.peek().cloned() {
        match tok {
            Tok::Ident(kw) if kw == "syntax" => {
                p.pos += 1;
                p.expect_sym('=');
                let _ = p.expect_str();
                p.expect_sym(';');
            }
            Tok::Ident(kw) if kw == "package" => {
                p.pos += 1;
                schema.package = p.dotted_name();
                p.expect_sym(';');
            }
            Tok::Ident(kw) if kw == "message" => {
                p.pos += 1;
                schema.messages.push(parse_message(&mut p));
            }
            Tok::Ident(kw) if kw == "service" => {
                p.pos += 1;
                schema.services.push(parse_service(&mut p));
            }
            other => panic!("proto reader: unexpected top-level token {other:?}"),
        }
    }
    schema
}

fn parse_message(p: &mut Parser) -> MessageDef {
    let name = p.expect_ident();
    p.expect_sym('{');
    let mut fields = Vec::new();
    while !matches!(p.peek(), Some(Tok::Sym('}'))) {
        let mut label = Label::LABEL_OPTIONAL;
        let mut optional_kw = false;
        if let Some(Tok::Ident(kw)) = p.peek().cloned() {
            if kw == "optional" {
                p.pos += 1;
                label = Label::LABEL_OPTIONAL;
                optional_kw = true;
            } else if kw == "repeated" {
                p.pos += 1;
                label = Label::LABEL_REPEATED;
            }
        }
        let type_name = p.dotted_name();
        let name = p.expect_ident();
        p.expect_sym('=');
        let number = p.expect_num() as i32;
        p.expect_sym(';');
        fields.push(FieldDef {
            label,
            optional_kw,
            type_name,
            name,
            number,
        });
    }
    p.expect_sym('}');
    MessageDef { name, fields }
}

fn parse_service(p: &mut Parser) -> ServiceDef {
    let name = p.expect_ident();
    p.expect_sym('{');
    let mut methods = Vec::new();
    while !matches!(p.peek(), Some(Tok::Sym('}'))) {
        let kw = p.expect_ident();
        if kw != "rpc" {
            panic!("proto reader: expected `rpc`, got `{kw}`");
        }
        let name = p.expect_ident();
        p.expect_sym('(');
        let input = p.dotted_name();
        p.expect_sym(')');
        let ret = p.expect_ident();
        if ret != "returns" {
            panic!("proto reader: expected `returns`, got `{ret}`");
        }
        p.expect_sym('(');
        let output = p.dotted_name();
        p.expect_sym(')');
        p.expect_sym(';');
        methods.push(MethodDef {
            name,
            input,
            output,
        });
    }
    p.expect_sym('}');
    ServiceDef { name, methods }
}

fn scalar_type(name: &str) -> Option<Type> {
    Some(match name {
        "double" => Type::TYPE_DOUBLE,
        "float" => Type::TYPE_FLOAT,
        "int64" => Type::TYPE_INT64,
        "uint64" => Type::TYPE_UINT64,
        "int32" => Type::TYPE_INT32,
        "fixed64" => Type::TYPE_FIXED64,
        "fixed32" => Type::TYPE_FIXED32,
        "bool" => Type::TYPE_BOOL,
        "string" => Type::TYPE_STRING,
        "bytes" => Type::TYPE_BYTES,
        "uint32" => Type::TYPE_UINT32,
        "sfixed32" => Type::TYPE_SFIXED32,
        "sfixed64" => Type::TYPE_SFIXED64,
        "sint32" => Type::TYPE_SINT32,
        "sint64" => Type::TYPE_SINT64,
        _ => return None,
    })
}

fn json_name(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for ch in name.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

impl Schema {
    /// Fully-qualified proto name for a type reference.
    fn qualify(&self, type_name: &str) -> String {
        if type_name.starts_with('.') {
            type_name.to_string()
        } else {
            format!(".{}.{}", self.package, type_name)
        }
    }

    fn to_descriptor(&self) -> FileDescriptorSet {
        let messages = self.messages.iter().map(|m| self.message_proto(m)).collect();

        let service = self
            .services
            .iter()
            .map(|s| ServiceDescriptorProto {
                name: Some(s.name.clone()),
                method: s
                    .methods
                    .iter()
                    .map(|m| MethodDescriptorProto {
                        name: Some(m.name.clone()),
                        input_type: Some(self.qualify(&m.input)),
                        output_type: Some(self.qualify(&m.output)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();

        FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("ipc.proto".to_string()),
                package: Some(self.package.clone()),
                message_type: messages,
                service,
                syntax: Some("proto3".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn message_proto(&self, msg: &MessageDef) -> DescriptorProto {
        let mut fields = Vec::new();
        // Synthetic oneofs (one per proto3 `optional` field), ordered after
        // any real oneofs; this schema has none.
        let mut oneofs: Vec<OneofDescriptorProto> = Vec::new();

        for f in &msg.fields {
            let scalar = scalar_type(&f.type_name);
            let mut field = FieldDescriptorProto {
                name: Some(f.name.clone()),
                number: Some(f.number),
                label: Some(f.label),
                json_name: Some(json_name(&f.name)),
                ..Default::default()
            };
            match scalar {
                Some(ty) => field.r#type = Some(ty),
                None => {
                    field.r#type = Some(Type::TYPE_MESSAGE);
                    field.type_name = Some(self.qualify(&f.type_name));
                }
            }
            if f.optional_kw && f.label == Label::LABEL_OPTIONAL {
                field.proto3_optional = Some(true);
                field.oneof_index = Some(oneofs.len() as i32);
                oneofs.push(OneofDescriptorProto {
                    name: Some(format!("_{}", f.name)),
                    ..Default::default()
                });
            }
            fields.push(field);
        }

        DescriptorProto {
            name: Some(msg.name.clone()),
            field: fields,
            oneof_decl: oneofs,
            ..Default::default()
        }
    }
}
