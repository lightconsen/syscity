import { Component, type ErrorInfo, type ReactNode } from "react";

interface ErrorBoundaryProps {
  /** What failed, for the recovery card ("message list", "document preview"). */
  area: string;
  children: ReactNode;
  /** Remount the subtree; defaults to resetting the boundary's own state. */
  onReset?: () => void;
}

interface ErrorBoundaryState {
  error: Error | null;
}

/**
 * Keeps one broken subtree from blanking the whole app.
 *
 * Without a boundary, any throw during render unmounts the entire React tree
 * and leaves an empty `#root` — visually identical to the WebView being torn
 * down, and unrecoverable without a restart. This confines the damage to the
 * area that threw (a single malformed tool result, a document that fails to
 * parse) and shows what happened.
 *
 * Deliberately dependency-free — no i18n, no store, no transport. A render
 * error can come from any of those, and the recovery UI must still draw.
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(`[ErrorBoundary] ${this.props.area} failed to render:`, error, info.componentStack);
  }

  private reset = () => {
    if (this.props.onReset) this.props.onReset();
    this.setState({ error: null });
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div className="flex flex-col items-center justify-center gap-3 p-6 text-center">
        <div className="text-sm font-medium text-primary">
          This part of the page could not be displayed.
        </div>
        <div className="text-[11px] text-secondary break-words max-w-full">
          <span className="font-mono">{this.props.area}</span>: {error.message || String(error)}
        </div>
        <button
          type="button"
          onClick={this.reset}
          className="px-3 py-1.5 rounded-md text-xs font-medium bg-primary-500 text-white hover:bg-primary-600 transition"
        >
          Try again
        </button>
      </div>
    );
  }
}
