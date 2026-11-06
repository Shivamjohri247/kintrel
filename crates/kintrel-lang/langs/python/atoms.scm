; Atoms for Python. Capture contract: see crates/kintrel-lang/src/lib.rs.
; Unnamed top-level statements are glue (SPEC §4.2 rule 6).

(function_definition name: (_) @name) @atom.def
(class_definition name: (_) @name body: (_) @body) @atom.class
(decorated_definition definition: (function_definition name: (_) @name)) @atom.def
(decorated_definition definition: (class_definition name: (_) @name body: (_) @body)) @atom.class
(import_statement name: (_) @name) @atom.import
(import_from_statement module_name: (_) @name) @atom.import
(expression_statement (assignment left: (identifier) @name)) @atom.var

(comment) @attach
