(function_definition
  declarator: (function_declarator declarator: (identifier) @name)
  body: (compound_statement) @body) @definition.function
(function_definition
  declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
  body: (compound_statement) @body) @definition.function
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @definition.enum
(type_definition declarator: (type_identifier) @name) @definition.type
(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro
