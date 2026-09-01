import * as React from "react";
import { cn } from "@/lib/utils";

export const Input = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement>>(
  ({ className, type, ...props }, ref) => (
    <input
      ref={ref}
      type={type}
      className={cn(
        "flex h-9 w-full rounded-md border border-[var(--input)] bg-transparent px-3 py-1 text-sm",
        "placeholder:text-muted-foreground/70 disabled:cursor-not-allowed disabled:opacity-50",
        "transition-colors focus-visible:border-[var(--ring)]",
        className,
      )}
      {...props}
    />
  ),
);
Input.displayName = "Input";

export const Textarea = React.forwardRef<
  HTMLTextAreaElement,
  React.TextareaHTMLAttributes<HTMLTextAreaElement>
>(({ className, ...props }, ref) => (
  <textarea
    ref={ref}
    className={cn(
      "flex w-full rounded-md border border-[var(--input)] bg-transparent px-3 py-2 text-sm",
      "placeholder:text-muted-foreground/70 disabled:cursor-not-allowed disabled:opacity-50",
      "font-mono leading-relaxed transition-colors focus-visible:border-[var(--ring)]",
      className,
    )}
    {...props}
  />
));
Textarea.displayName = "Textarea";
