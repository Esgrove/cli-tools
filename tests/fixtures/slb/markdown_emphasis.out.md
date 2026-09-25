# Emphasis

A paragraph that runs well past the configured limit and ends with **a bold phrase that must stay together** even
though the author split it by hand across two separate lines.

Short **bold split** here.

Use _an italic phrase_ in a sentence that is long enough to need a break somewhere,
and the break has to land outside the emphasis rather than inside it.

Bold and italic together in ***one long emphasized phrase that must not break*** inside a line of prose
that is long enough to force the formatter to choose a break point.

Alternate markers __bold with underscores__ and _italic_ mixed into one line that runs past the configured limit,
so that at least one break has to happen somewhere in the sentence.

An asterisk used as multiplication in a * b stays breakable,
and so does a stray _underscore in the middle of the prose, because neither marker is ever closed.

Words with snake_case_names and a leading _private field do not open an emphasis span,
even in a line of prose that is long enough to need a break.

Inline `code with * and _ inside` is one atom, and a [link with **bold** text](https://example.com/docs) is one as well,
so neither of them can be broken apart.

Strikethrough ~~text that is struck out~~ in a sentence long enough
that the formatter has to pick a break point somewhere along the line.

A struck out ~~phrase that was split by hand~~ is joined back together.
