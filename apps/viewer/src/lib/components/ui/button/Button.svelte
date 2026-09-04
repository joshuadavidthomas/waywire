<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLButtonAttributes } from "svelte/elements";

  type Variant = "primary" | "secondary" | "ghost";
  type Props = HTMLButtonAttributes & {
    children: Snippet;
    variant?: Variant;
  };

  let {
    children,
    variant = "secondary",
    class: className = "",
    type = "button",
    ...rest
  }: Props = $props();

  const variants: Record<Variant, string> = {
    primary:
      "bg-action text-action-ink hover:bg-action-hover focus-visible:outline-action",
    secondary:
      "border border-edge-strong bg-panel text-ink hover:bg-white/6 focus-visible:outline-action disabled:bg-panel",
    ghost:
      "text-ink-secondary hover:bg-white/6 hover:text-ink focus-visible:outline-action",
  };
</script>

<button
  {type}
  class={`relative inline-flex h-11 items-center justify-center gap-2 rounded-md px-3 text-base font-medium whitespace-nowrap outline-none focus-visible:outline-2 focus-visible:outline-offset-2 disabled:cursor-not-allowed disabled:opacity-40 sm:h-9 sm:text-sm ${variants[variant]} ${className}`}
  {...rest}
>
  {@render children()}
  <span
    class="pointer-events-none absolute top-1/2 left-1/2 size-[max(100%,3rem)] -translate-1/2 pointer-fine:hidden"
    aria-hidden="true"
  ></span>
</button>
