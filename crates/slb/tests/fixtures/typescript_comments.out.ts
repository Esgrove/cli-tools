/**
 * Parses the configuration file and returns the resolved settings.
 * The caller handles the errors.
 * The parser is lenient.
 * Unknown keys are ignored, and missing keys fall back to the defaults.
 */
export function parseConfig(source: string): Config {
    // The regular expression below must not be treated as a comment even though it contains slashes.
    const pattern = /^\/\/\s*(.*)$/;
    // template literals may contain markers
    const template = `a // b ${source} c`;
    // default width
    const width = 80;
    // TODO: support the legacy format as well, which is still used by a few of the older projects.
    return { pattern, template, width };
}

/**
 * Builds a multi-line template literal whose interpolation contains a quoted backtick.
 * The inner backtick must not close the outer template literal prematurely.
 */
export function formatPrompt(flag: boolean, total: number, count: number): string {
    const snippet = `prefix ${flag ? '`' : '"'}
// inside template literal after quoted backtick; semicolons stay verbatim
// wrapped lines inside the template
// are never reflowed.
`;
    const ratio = total / count + `
// inside template literal after division; must stay untouched
`;
    // combine both fragments
    return snippet + ratio;
}

/**
 * Short summary.
 *
 * | Option | Meaning |
 * | ------ | ------- |
 * | width  | columns |
 *
 * See https://example.com/docs/configuration/reference for the full list of the supported options.
 */
export const defaults: Config = { width: 80 };
