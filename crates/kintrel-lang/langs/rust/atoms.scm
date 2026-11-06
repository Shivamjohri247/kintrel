; Atoms for Rust. See crates/kintrel-lang/src/lib.rs for the capture contract:
;   @atom.<kind>  the item; <kind> becomes the key segment kind
;   @name         name text (several @name captures: source span from first to last)
;   @body         container body; its direct child atoms are members (one level deep)
;   @attach       comments/attributes that attach to the item that follows them

(function_item name: (_) @name) @atom.fn
(function_signature_item name: (_) @name) @atom.fn
(struct_item name: (_) @name) @atom.struct
(enum_item name: (_) @name) @atom.enum
(union_item name: (_) @name) @atom.union
(const_item name: (_) @name) @atom.const
(static_item name: (_) @name) @atom.static
(type_item name: (_) @name) @atom.type
(associated_type name: (_) @name) @atom.type
(macro_definition name: (_) @name) @atom.macro
(extern_crate_declaration name: (_) @name) @atom.use

(use_declaration argument: (scoped_use_list path: (_) @name)) @atom.use
(use_declaration argument: [(identifier) (scoped_identifier) (use_as_clause) (use_wildcard) (use_list)] @name) @atom.use

(impl_item trait: (_) @name type: (_) @name body: (_) @body) @atom.impl
(impl_item !trait type: (_) @name body: (_) @body) @atom.impl
(trait_item name: (_) @name body: (_) @body) @atom.trait
(mod_item name: (_) @name body: (_) @body) @atom.mod
(mod_item name: (_) @name !body) @atom.mod

(line_comment) @attach
(block_comment) @attach
(attribute_item) @attach
