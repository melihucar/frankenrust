<?php
// Small, realistic response: touches the output buffer and header layer
// without meaningful compute. Sits between `noop` and `compute`.
header('Content-Type: application/json');
echo json_encode([
    'message' => 'Hello World',
    'server'  => $_SERVER['SERVER_SOFTWARE'] ?? 'unknown',
    'path'    => $_SERVER['REQUEST_URI'] ?? '/',
    'items'   => range(1, 16),
]);
