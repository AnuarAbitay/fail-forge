/// Decode platform print terminators for semantic program-output assertions.
String semanticStdout(String output) => output.replaceAll('\r\n', '\n');
