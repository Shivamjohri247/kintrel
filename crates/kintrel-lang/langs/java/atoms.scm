; Atoms for Java. Capture contract: see crates/kintrel-lang/src/lib.rs.
; Overloaded methods are told apart by ordinal (m, m#1).

(class_declaration name: (_) @name body: (_) @body) @atom.class
(interface_declaration name: (_) @name body: (_) @body) @atom.interface
(enum_declaration name: (_) @name body: (_) @body) @atom.enum
(record_declaration name: (_) @name body: (_) @body) @atom.record
(annotation_type_declaration name: (_) @name body: (_) @body) @atom.annotation
(import_declaration (_) @name) @atom.import

; members
(method_declaration name: (_) @name) @atom.method
(constructor_declaration name: (_) @name) @atom.constructor
(compact_constructor_declaration name: (_) @name) @atom.constructor
(field_declaration declarator: (variable_declarator name: (_) @name)) @atom.field
(constant_declaration declarator: (variable_declarator name: (_) @name)) @atom.field

(line_comment) @attach
(block_comment) @attach
