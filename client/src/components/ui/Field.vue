<script setup lang="ts">
// 注意：根节点必须是 div 而不是 label。
// 若用 label 包裹，字段内部的按钮（如密码框的眼睛切换）点击会被 label 转发到
// 第一个表单控件上，导致按钮失效。
withDefaults(defineProps<{ label?: string; hint?: string; required?: boolean }>(), { required: false });
</script>

<template>
  <div class="block">
    <span v-if="label" class="mb-1.5 flex items-center gap-1 text-[12.5px] font-medium text-ink-2">
      {{ label }}<span v-if="required" class="text-err-500">*</span>
    </span>
    <slot />
    <span v-if="hint" class="mt-1 block text-[11.5px] leading-4 text-ink-3">{{ hint }}</span>
  </div>
</template>
