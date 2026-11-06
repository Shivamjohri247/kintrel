; Atoms for JavaScript and JSX. Capture contract: see crates/kintrel-lang/src/lib.rs.

(function_declaration name: (_) @name) @atom.function
(export_statement declaration: (function_declaration name: (_) @name)) @atom.function
(generator_function_declaration name: (_) @name) @atom.function
(export_statement declaration: (generator_function_declaration name: (_) @name)) @atom.function

(class_declaration name: (_) @name body: (_) @body) @atom.class
(export_statement declaration: (class_declaration name: (_) @name body: (_) @body)) @atom.class

(lexical_declaration (variable_declarator name: (_) @name)) @atom.const
(export_statement declaration: (lexical_declaration (variable_declarator name: (_) @name))) @atom.const
(variable_declaration (variable_declarator name: (_) @name)) @atom.var
(export_statement declaration: (variable_declaration (variable_declarator name: (_) @name))) @atom.var

(import_statement source: (_) @name) @atom.import
(export_statement source: (_) @name) @atom.export

; class members
(method_definition name: (_) @name) @atom.method
(field_definition property: (_) @name) @atom.field

(comment) @attach
(decorator) @attach

; CommonJS: `exports.name = ...`
((expression_statement (assignment_expression left: (member_expression object: (identifier) @_obj property: (_) @name))) @atom.export
 (#eq? @_obj "exports"))
