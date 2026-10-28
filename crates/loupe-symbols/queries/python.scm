(function_definition name: (identifier) @name body: (block) @body) @definition.function
(class_definition name: (identifier) @name body: (block) @body) @definition.class
(function_definition
  name: (identifier) @name
  body: (block . (expression_statement (string) @doc))) @definition.function
(class_definition
  name: (identifier) @name
  body: (block . (expression_statement (string) @doc))) @definition.class
