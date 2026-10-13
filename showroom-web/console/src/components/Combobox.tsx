import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { cn } from "cn";

/** Combobox：可输入过滤的下拉选择（对齐现版「输入前缀过滤」自动补全形态） */
export function Combobox({
  value, onValueChange, options, placeholder, searchPlaceholder,
  className, disabled, emptyText = "无匹配项", ariaLabel,
}: {
  value: string;
  onValueChange: (v: string) => void;
  options: { value: string; label: string }[];
  placeholder?: string;
  searchPlaceholder?: string;
  className?: string;
  disabled?: boolean;
  emptyText?: string;
  ariaLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button" variant="outline" role="combobox" aria-expanded={open}
          aria-label={ariaLabel} disabled={disabled}
          className={cn("w-44 justify-between font-mono text-xs font-normal", className)}
        >
          <span className="truncate">{value || placeholder}</span>
          <ChevronDownIcon data-icon="inline-end" className="opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-(--radix-popover-trigger-width) p-0" align="start">
        <Command>
          <CommandInput placeholder={searchPlaceholder} />
          <CommandList>
            <CommandEmpty>{emptyText}</CommandEmpty>
            <CommandGroup>
              {options.map((o) => (
                <CommandItem
                  key={o.value} value={o.value}
                  className="font-mono text-xs"
                  onSelect={(v) => { onValueChange(v); setOpen(false); }}
                >
                  <CheckIcon className={cn(value === o.value ? "opacity-100" : "opacity-0")} />
                  <span className="truncate">{o.label}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
