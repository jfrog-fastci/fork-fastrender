#include <stdio.h>
#include <stdlib.h>

#include <llvm-c/Core.h>
#include <llvm-c/Error.h>
#include <llvm-c/IRReader.h>
#include <llvm-c/Support.h>
#include <llvm-c/Target.h>
#include <llvm-c/TargetMachine.h>
#include <llvm-c/Transforms/PassBuilder.h>

static void die(const char *msg, char *llvm_msg) {
  fprintf(stderr, "%s: %s\n", msg, llvm_msg ? llvm_msg : "(null)");
  if (llvm_msg) {
    LLVMDisposeMessage(llvm_msg);
  }
  exit(1);
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s <passes-pipeline> <input.ll>\n", argv[0]);
    return 2;
  }

  const char *pipeline = argv[1];
  const char *input_path = argv[2];

  LLVMInitializeNativeTarget();
  LLVMInitializeNativeAsmPrinter();
  LLVMInitializeNativeAsmParser();

  LLVMMemoryBufferRef buf = NULL;
  char *msg = NULL;
  if (LLVMCreateMemoryBufferWithContentsOfFile(input_path, &buf, &msg) != 0) {
    die("LLVMCreateMemoryBufferWithContentsOfFile failed", msg);
  }

  LLVMContextRef ctx = LLVMContextCreate();
  LLVMModuleRef module = NULL;
  if (LLVMParseIRInContext(ctx, buf, &module, &msg) != 0) {
    die("LLVMParseIRInContext failed", msg);
  }
  // Note: `LLVMParseIRInContext` takes ownership of `buf` on success.

  char *triple = LLVMGetDefaultTargetTriple();
  LLVMTargetRef target;
  if (LLVMGetTargetFromTriple(triple, &target, &msg) != 0) {
    die("LLVMGetTargetFromTriple failed", msg);
  }

  LLVMTargetMachineRef tm = LLVMCreateTargetMachine(
      target, triple, "", "", LLVMCodeGenLevelDefault, LLVMRelocDefault,
      LLVMCodeModelDefault);
  if (!tm) {
    die("LLVMCreateTargetMachine failed", NULL);
  }

  LLVMSetTarget(module, triple);

  LLVMPassBuilderOptionsRef opts = LLVMCreatePassBuilderOptions();

  LLVMErrorRef err = LLVMRunPasses(module, pipeline, tm, opts);
  if (err) {
    char *err_msg = LLVMGetErrorMessage(err);
    fprintf(stderr, "LLVMRunPasses returned error: %s\n", err_msg);
    LLVMDisposeErrorMessage(err_msg);
    LLVMConsumeError(err);
    return 1;
  }

  char *out_ir = LLVMPrintModuleToString(module);
  printf("%s", out_ir);
  LLVMDisposeMessage(out_ir);

  LLVMDisposePassBuilderOptions(opts);
  LLVMDisposeTargetMachine(tm);
  LLVMDisposeMessage(triple);
  LLVMDisposeModule(module);
  LLVMContextDispose(ctx);

  return 0;
}
