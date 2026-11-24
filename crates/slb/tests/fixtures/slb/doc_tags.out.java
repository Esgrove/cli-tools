package example;

/** Client for the archive service. */
class ArchiveClient {
    /**
     * Creates and sends the HTTP request which marks an archive entry
     * as a favourite of the signed in reader.
     * @param archiveEntryIdentifier The id of the archive entry which will be added
     *                               to the favourites of the signed in reader.
     */
    void addFavourite(String archiveEntryIdentifier) {}

    /**
     * Creates and sends the HTTP request which removes an archive entry from the favourites of the signed in reader.
     * @param archiveEntryIdentifier The id of the archive entry which will be removed
     *                               from the favourites of the signed in reader.
     */
    void removeFavourite(String archiveEntryIdentifier) {}

    /**
     * Converts a hexadecimal string into a byte array.
     *
     * @param hex   A hexadecimal string where every two characters represent one byte.
     *              The string must have an even length.
     * @return A byte array holding the decoded bytes.
     * @throws IllegalArgumentException if the input string has an odd length
     *                                  or contains invalid hexadecimal characters.
     */
    byte[] decode(String hex) {
        return null;
    }

    /**
     * Deletes a file at {@code devicePath} on the session device through the {@code mobile: deleteFile} script.
     *
     * Deletion is idempotent, so a caller may retry the same {@code devicePath}
     * after a timeout without checking the result of the first attempt.
     */
    void deleteFile(String devicePath) {}

    /**
     * Restarts the session device and waits until it responds again, and the timeout is fixed and not configurable.
     * See {@link #deleteFile} for the script that runs on shutdown.
     *
     * @see #deleteFile
     */
    void restart() {}

    /**
     * Applies the options string to the session device.
     * For example, a value like {@code region: eu-west-1, timeout: 30s, retries: 3, verbose: true} is parsed
     * before it mutates any persisted state on the device.
     */
    void applyOptions(String options) {}
}
