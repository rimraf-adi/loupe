(function_declaration name: (identifier) @name body: (block) @body) @definition.function
(method_declaration name: (field_identifier) @name body: (block) @body) @definition.method
(type_spec name: (type_identifier) @name type: (struct_type)) @definition.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @definition.interface
(type_spec name: (type_identifier) @name) @definition.type
(const_spec name: (identifier) @name) @definition.constant
