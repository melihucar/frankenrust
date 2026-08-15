<?php
// Fixture for tests/directory_index_cgi_vars.rs: dumps the four CGI path
// variables `server.rs`'s directory-index rewrite is responsible for, so the
// test can assert them against the values observed from the pinned upstream
// container (see `resolve_directory_index`'s doc comment in `server.rs`).
foreach (['DOCUMENT_URI', 'PATH_INFO', 'SCRIPT_NAME', 'SCRIPT_FILENAME'] as $name) {
    echo $name, '=', $_SERVER[$name] ?? '', "\n";
}
