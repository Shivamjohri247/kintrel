; Atoms for TypeScript and TSX. Capture contract: see crates/kintrel-lang/src/lib.rs.
; Exported declarations are wrapped in export_statement, the direct child of the file root.

(function_declaration name: (_) @name) @atom.function
(export_statement declaration: (function_declaration name: (_) @name)) @atom.function
(generator_function_declaration name: (_) @name) @atom.function
(export_statement declaration: (generator_function_declaration name: (_) @name)) @atom.function
(function_signature name: (_) @name) @atom.function
(export_statement declaration: (function_signature name: (_) @name)) @atom.function

(class_declaration name: (_) @name body: (_) @body) @atom.class
(export_statement declaration: (class_declaration name: (_) @name body: (_) @body)) @atom.class
(abstract_class_declaration name: (_) @name body: (_) @body) @atom.class
(export_statement declaration: (abstract_class_declaration name: (_) @name body: (_) @body)) @atom.class
(interface_declaration name: (_) @name body: (_) @body) @atom.interface
(export_statement declaration: (interface_declaration name: (_) @name body: (_) @body)) @atom.interface

(type_alias_declaration name: (_) @name) @atom.type
(export_statement declaration: (type_alias_declaration name: (_) @name)) @atom.type
(enum_declaration name: (_) @name) @atom.enum
(export_statement declaration: (enum_declaration name: (_) @name)) @atom.enum
(lexical_declaration (variable_declarator name: (_) @name)) @atom.const
(export_statement declaration: (lexical_declaration (variable_declarator name: (_) @name))) @atom.const
(variable_declaration (variable_declarator name: (_) @name)) @atom.var
(export_statement declaration: (variable_declaration (variable_declarator name: (_) @name))) @atom.var
(expression_statement (internal_module name: (_) @name body: (_) @body)) @atom.namespace

(import_statement source: (_) @name) @atom.import
(export_statement source: (_) @name) @atom.export

; members of classes and interfaces
(method_definition name: (_) @name) @atom.method
(abstract_method_signature name: (_) @name) @atom.method
(method_signature name: (_) @name) @atom.method
(public_field_definition name: (_) @name) @atom.field
(property_signature name: (_) @name) @atom.field

(comment) @attach
(decorator) @attach
