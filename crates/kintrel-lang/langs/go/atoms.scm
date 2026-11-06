; Atoms for Go. Capture contract: see crates/kintrel-lang/src/lib.rs.
; A method is named by receiver and name, "(t *T) String", so methods of different types with
; the same name do not collide. Import blocks get a fixed name: a file usually has one.

(function_declaration name: (_) @name) @atom.func
(method_declaration receiver: (_) @name name: (_) @name) @atom.method
(type_declaration (type_spec name: (_) @name)) @atom.type
(type_declaration (type_alias name: (_) @name)) @atom.type
(const_declaration (const_spec name: (_) @name)) @atom.const
(var_declaration (var_spec name: (_) @name)) @atom.var
(import_declaration) @atom.import

(comment) @attach
