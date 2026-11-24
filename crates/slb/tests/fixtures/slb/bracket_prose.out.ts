/**
 * Maps a database row to the API model.
 * The query compiler only handles primitives (string, number, boolean, null, Date and Buffer).
 * Arrays and JSON objects are passed through unchanged.
 *
 * The factories build model objects carrying the derived fields
 * (`value`, `carbonEmissions`, `totalAccumulation`) that the mapper does not know about.
 */
export function mapRow(row: Row): Model {
    // The chips reuse the same semantic colors as the user side (see `customColors` and the maps in `ItemCard.tsx`)
    // so a status reads identically on both sides.
    const colors = customColors;
    // Note that the search path is set at the role level (the admin role has search_path = public),
    // so no explicit statement is needed here.
    return toModel(row, colors);
}

/**
 * Formats the model for display; see {@link mapRow} for how the fields are derived
 * from the raw row, and {@link formatDate} for how the timestamp fields are rendered.
 */
export function formatModel(model: Model): string {
    // The fallback label {unknown} is only shown when every other field is missing,
    // which should not happen for a row that passed validation.
    return model.label ?? "{unknown}";
}
