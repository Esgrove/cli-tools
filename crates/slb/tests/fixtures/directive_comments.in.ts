// A trailing comment that is a directive for another tool stays on the line it applies to.
// Every other trailing comment moves onto its own line above the code.

const typed = load(); // @ts-expect-error the loader has no type definitions yet
const allowed = legacy(); // allow(deprecated) until the next major release
const formatted = raw(); // biome-ignore format: the table is aligned by hand
const linted = clippy(); // clippy::too_many_lines is expected in generated code
const spelled = word(); // codespell:ignore teh
const checked = text(); // cspell:disable-line
const denied = unsafe(); // deny(warnings) applies to this call only
const quiet = logger(); // eslint-disable-line no-console
const aligned = matrix(); // fmt: skip
const kotlin = bridge(); // ktlint-disable max-line-length
const untyped = dynamic(); // mypy: ignore-errors
const unchecked = cast(); // nolint:errcheck
const skipped = imported(); // noqa: F401
const secret = token(); // nosec B105
const sonar = complex(); // NOSONAR
const pragma = once(); // pragma: no cover
const pretty = layout(); // prettier-ignore
const pylint = unused(); // pylint: disable=unused-variable
const pyright = optional(); // pyright: ignore[reportOptionalMemberAccess]
const ruff = legacyImport(); // ruff: noqa: E402
const kept = macro(); // rustfmt::skip
const safe = pointer(); // SAFETY: the pointer is valid for the whole call
const shell = script(); // shellcheck disable=SC2086
const spell = jargon(); // spellchecker: disable-line
const swift = bridgeSwift(); // swiftlint:disable:this force_cast
const hinted = parse(); // type: ignore
const yaml = document(); // yamllint disable-line rule:line-length

const moved = compute(); // this ordinary note moves above the code
