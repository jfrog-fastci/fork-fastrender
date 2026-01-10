// META: script=/resources/testharness.js

// This test intentionally never reports a result; the runner should terminate it as a timeout.
//
// Use an empty statement body so the VM does not spend most of its budget repeatedly allocating a
// fresh lexical environment for an empty block.
while (true);
