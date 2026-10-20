(method) @keyword

(header
  name: (_) @property)

(variable_declaration
  name: (identifier) @variable)

(variable
  name: (_) @variable.special)

(comment
  "=" @operator)
(variable_declaration
  "=" @operator)

(comment
  "@" @keyword
  name: (_) @keyword)

(request
  url: (_) @link_uri)

(http_version) @constant

(status_code) @number
(status_text) @string

[
  "{{"
  "}}"
] @punctuation.bracket

(header
  ":" @punctuation.delimiter)

(external_body
  path: (_) @string.special)

[
  (comment)
  (request_separator)
] @comment
