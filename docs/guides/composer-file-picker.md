# Composer file picker

Type `@` in the interactive composer to open a list of workspace files. Type part of a path to filter the list. Use the arrow keys to select a file, then press Enter to insert its reference. Enter selects the file; it does not send the prompt. The reference is inserted at the cursor and is quoted when the path needs quoting.

Press Escape to close the list and keep the draft. If no paths match, the list reports that it has no results. A file removed before selection is not inserted.

The picker lists files below the admitted workspace root. It respects `.gitignore` files and skips symbolic links. The current read perimeter applies to the walk. The picker reads only ignore-rule files; it never reads candidate file contents. A file reference does not grant permission to read the file. The list is bounded, so it may not contain every file in a large workspace.

The walk checks paths before it opens them. If a directory changes to a symbolic link between the check and the open, the walk can follow that link outside the workspace or read an external ignore-rule file. This is the same path-based check/open window documented for guarded filesystem access.
