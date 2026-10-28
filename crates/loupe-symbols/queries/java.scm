(class_declaration name: (identifier) @name body: (class_body) @body) @definition.class
(interface_declaration name: (identifier) @name body: (interface_body) @body) @definition.interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @definition.enum
(record_declaration name: (identifier) @name body: (_) @body) @definition.class
(method_declaration name: (identifier) @name body: (block) @body) @definition.method
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name body: (constructor_body) @body) @definition.method
