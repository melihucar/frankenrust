<?php
// CPU-bound control. The PHP interpreter dominates wall time here, so both
// servers MUST land inside the noise band. If this benchmark shows a large
// difference, the harness is broken -- not the server.
$acc = 0.0;
for ($i = 1; $i < 200000; $i++) {
    $acc += sqrt($i) / ($i % 7 + 1);
}
echo number_format($acc, 4);
