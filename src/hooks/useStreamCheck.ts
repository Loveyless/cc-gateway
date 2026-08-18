import type { AppId } from "@/lib/api";

export function useStreamCheck(_appId: AppId) {
  return {
    checkProvider: async (_id?: string, _name?: string) => null,
    isChecking: (_id?: string) => false,
  };
}
