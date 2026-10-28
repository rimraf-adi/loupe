(function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(generator_function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(class_declaration name: (identifier) @name body: (class_body) @body) @definition.class
(method_definition name: (_) @name body: (statement_block) @body) @definition.method
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function body: (_) @body))) @definition.function
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (function_expression body: (statement_block) @body))) @definition.function
