(function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(generator_function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(class_declaration name: (type_identifier) @name body: (class_body) @body) @definition.class
(abstract_class_declaration name: (type_identifier) @name body: (class_body) @body) @definition.class
(method_definition name: (_) @name body: (statement_block) @body) @definition.method
(method_signature name: (_) @name) @definition.method
(abstract_method_signature name: (_) @name) @definition.method
(interface_declaration name: (type_identifier) @name body: (_) @body) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name body: (_) @body) @definition.enum
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function body: (_) @body))) @definition.function
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (function_expression body: (statement_block) @body))) @definition.function
