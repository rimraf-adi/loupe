; Captures: @definition.<kind> on the whole item, @name on its name, optional @body on the
; block/body (the signature is the text before it) and @doc for an explicit doc string.
(function_item name: (identifier) @name body: (block) @body) @definition.function
(function_signature_item name: (identifier) @name) @definition.function
(struct_item name: (type_identifier) @name) @definition.struct
(union_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(trait_item name: (type_identifier) @name body: (declaration_list) @body) @definition.trait
(impl_item type: (_) @name body: (declaration_list) @body) @definition.impl
(mod_item name: (identifier) @name) @definition.module
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(type_item name: (type_identifier) @name) @definition.type
(macro_definition name: (identifier) @name) @definition.macro
