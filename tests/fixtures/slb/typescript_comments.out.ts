/**
 * Parses the configuration file and returns the resolved settings. The caller handles the errors.
 * The parser is lenient, unknown keys are ignored, and missing keys fall back to the defaults.
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
 * Short summary.
 *
 * | Option | Meaning |
 * | ------ | ------- |
 * | width  | columns |
 *
 * See https://example.com/docs/configuration/reference for the full list of the supported options.
 */
export const defaults: Config = { width: 80 };
