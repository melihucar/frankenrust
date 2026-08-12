<?php
// Pure server overhead. PHP does essentially nothing, so any measured
// difference between servers is the request pipeline itself: accept, parse,
// SAPI startup/shutdown, header assembly, write, teardown. This is the only
// app in the suite where we should EXPECT the Rust port to differ.
