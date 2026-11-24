# Abbreviations and sentence ends

## Abbreviations

An abbreviation ends with a period but never ends a sentence,
so a long line never breaks after one even when a capitalized word or a number follows it.

Pick a plain format, e.g. Markdown or reStructuredText, for the documentation of every package in the repository, and keep it next to the code.

The tool only reads tracked files, i.e. Git decides what is formatted, so every ignored directory is skipped without a single warning on the terminal.

The report lists warnings, notes, hints, etc. Every entry links back to the line of the file that caused it, which saves a lot of time during a review.

The benchmark compares the Rust vs. Go implementations of the parser on the same machine, and the results are written to the target directory afterwards.

The option behaves like the one in the shell, cf. Bash manual section on globbing, and it accepts exactly the same patterns as the interactive shell does.

The full test suite takes approx. 10 minutes on a fast machine, so the pull request job only runs the tests of the packages that the change touched.

The oldest archive dates from ca. 1990 and still opens, as the format has not changed since then, although the tool that wrote it was retired long ago.

The crash was first reported in issue no. 42 by a user on the forum, and the fix shipped in the next patch release together with a new regression test.

The layout of the pipeline is shown in fig. 3 of the design document, where every stage is drawn as a box and every artifact is drawn as an arrow.

The error term follows from eq. 7 of the paper once the sample size is large enough, which is why the tool refuses to estimate it for tiny inputs.

The algorithm was described by Dr. Virtanen and Mr. Korhonen at the conference, and Ms. Laine later extended it to streams of unbounded length.

The original prototype was written by Mrs. Nieminen during a workshop in St. Petersburg, where the team met to plan the first public release of the tool.

The keynote honoured Martin Luther King Jr. Day and the retirement of John Smith Sr. From the board, and the rest of the day was spent on workshops.

The library is maintained by Example Inc. Engineers in Helsinki together with Sample Ltd. Contractors abroad, who handle the releases for the other platforms.

The mirror is operated by Example Co. Staff on behalf of the project, and Prof. Mäkinen chairs the committee that reviews every change to the file format.

The approach follows the method of Smith et al. From the original paper, and the first and second thresholds are ten and twenty, resp. Their sum is thirty.

The job waits for 5 min. Then it retries with a limit of max. 3 attempts, and the report shows the avg. Latency of every attempt incl. VAT and excl. Weekends.

## Sentence ends

A period, an exclamation mark, or a question mark ends a sentence, so a long line breaks after it.

The build passed on the very first try, and every later stage of the pipeline reused the cache that the first one wrote. Nobody waited.

The build passed on the very first try, and every later stage of the pipeline reused the cache that the first one wrote! Nobody waited.

Did the build pass on the very first try, and did every later stage of the pipeline reuse the cache that the first one wrote? It did.

## Ellipses

An ellipsis trails off instead of ending a sentence, so a long line does not break after one.

The job waited for the lock... Then it retried with a longer timeout, and the second attempt finally went through after a few more minutes of waiting.

The job waited for the lock… Then it retried with a longer timeout, and the second attempt finally went through after a few more minutes of waiting.
