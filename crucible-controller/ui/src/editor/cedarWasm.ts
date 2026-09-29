import init, { validate, type ValidationAnswer } from '@cedar-policy/cedar-wasm/web';

export interface CedarValidator {
  validate: (policies: string, schema: string) => ValidationAnswer;
}

/// Cedar compiled to WebAssembly: several megabytes, so only the Policy page imports this, and only
/// dynamically.
export async function loadCedar(): Promise<CedarValidator> {
  await init();
  return {
    validate: (policies, schema) =>
      validate({ schema, policies: { staticPolicies: policies }, validationSettings: { mode: 'strict' } }),
  };
}
