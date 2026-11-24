# Inline HTML

A paragraph that opens with an inline tag is still prose, so it is reflowed like any other paragraph.

<a href="https://example.com">The project site</a> lists every release of the tool,
and each entry links to the notes that describe it.

<abbr title="Continuous integration">CI</abbr> runs the whole test suite on every push,
and a failing job blocks the merge until it is fixed.

<b>Warning</b> the cleanup job removes every file in the scratch directory,
so nothing that has to survive should ever be kept there.

<br> The line break tag starts this paragraph of the document,
and the rest of its text is long enough to need a break somewhere in it.

<code>slb</code> checks the comments of every supported language,
and it reflows the ones that break in the middle of a clause.

<em>Every</em> option can also be set in the configuration file,
and the command line flag wins when both of them are given.

<i>Note</i> the formatter never touches code,
so a comment that holds a short example keeps its layout exactly as the author wrote it.

<kbd>Ctrl</kbd> and the arrow keys move between the violations in the report,
and the enter key opens the file at the line.

<mark>Highlighted</mark> text stays together with the words around it,
and the paragraph still breaks at a clause boundary.

<p>A paragraph tag at the start of the line does not make this a block of raw markup,
so the text after it is reflowed as ordinary prose.

<small>Small print</small> at the start of a paragraph reads like any other sentence,
and it breaks at the same boundaries.

<span class="note">Spans</span> are used for styling only,
so they never decide where the lines of the paragraph around them break.

<strong>Important</strong> the release job signs every artifact,
and an unsigned artifact is rejected by the download page.

<sub>Subscript</sub> text is rare in comments,
but a paragraph that opens with it is still reflowed like all of the other ones here.

<sup>Superscript</sup> text marks a footnote in some documents,
and the paragraph after it breaks at its clause boundaries.

<u>Underlined</u> text is discouraged on the web,
but the formatter treats the paragraph that holds it as prose all the same anyway.

## Raw HTML blocks

A block level tag such as a div marks raw markup, which is kept exactly as it was written.

<div class="warning">The cleanup job removes every file in the scratch directory, so nothing that has to survive should be kept there.</div>
