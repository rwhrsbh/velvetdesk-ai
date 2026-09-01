/** Callbacks every modal needs to push its result back into the app shell. */
export interface ModalDeps {
  /** reload profiles + men + settings + pending and re-render */
  refresh: () => Promise<void>;
  selectProfile: (modelId: string) => Promise<void>;
  selectMan: (manId: string | null) => Promise<void>;
}
