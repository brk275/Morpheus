param([string] $Mode = 'basic')

# Only fixed Windows system DLL exports are called. Reader initialization also
# uses bundled in-process Roslyn on fixed source (separate compiler admission).
# No external executable, batch file, SDK installer or shell command is invoked.
function New-HostReadDelegate([string] $Name, [Type] $Result, [Type[]] $Arguments) {
    $nameInfo = [Reflection.AssemblyName]::new('MorpheusHostRead_' + [Guid]::NewGuid().ToString('N'))
    $assembly = [Reflection.Emit.AssemblyBuilder]::DefineDynamicAssembly($nameInfo, [Reflection.Emit.AssemblyBuilderAccess]::Run)
    $module = $assembly.DefineDynamicModule('ReadOnly')
    $type = $module.DefineType('NativeCall', [Reflection.TypeAttributes]'Public,Sealed', [MulticastDelegate])
    $ctor = $type.DefineConstructor([Reflection.MethodAttributes]'Public,HideBySig,RTSpecialName',
        [Reflection.CallingConventions]::Standard, [Type[]]@([object], [IntPtr]))
    $ctor.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    $invoke = $type.DefineMethod('Invoke', [Reflection.MethodAttributes]'Public,HideBySig,NewSlot,Virtual', $Result, $Arguments)
    $invoke.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    $delegateType = $type.CreateTypeInfo().AsType()
    return [Runtime.InteropServices.Marshal]::GetDelegateForFunctionPointer(
        [Runtime.InteropServices.NativeLibrary]::GetExport($script:HostKernel, $Name), $delegateType)
}

# The repeated comparison executes as owned managed IL, like the delegate
# declarations above. It calls no compiler, external tool or new native export.
# Status is closed: 0=equal, 1=type, 2=identity, 3=reparse, 4=changed.
function New-HostComparisonInvoker([Delegate] $TypeCall, [Delegate] $InfoCall) {
    $typeMethod = $TypeCall.GetType().GetMethod('Invoke')
    $infoMethod = $InfoCall.GetType().GetMethod('Invoke')
    $method = [Reflection.Emit.DynamicMethod]::new('CompareHostReadIdentity', [int],
        [Type[]]@([Delegate], [Delegate], [IntPtr], [IntPtr], [byte[]], [object], [bool]))
    $il = $method.GetILGenerator()
    $expected = $il.DeclareLocal([byte[]])
    $typeOk = $il.DefineLabel(); $infoOk = $il.DefineLabel()
    $notReparse = $il.DefineLabel(); $identityFailure = $il.DefineLabel()
    $changed = $il.DefineLabel(); $equal = $il.DefineLabel()
    $copy = [Runtime.InteropServices.Marshal].GetMethod('Copy',
        [Type[]]@([IntPtr], [byte[]], [int], [int]))
    $u32 = [BitConverter].GetMethod('ToUInt32', [Type[]]@([byte[]], [int]))
    $u64 = [BitConverter].GetMethod('ToUInt64', [Type[]]@([byte[]], [int]))

    # GetFileType(handle) == 1, then GetFileInformationByHandle(handle, memory)
    # != 0. These original calls and their first-failure order are unchanged.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Castclass, $TypeCall.GetType())
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $typeMethod)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Beq, $typeOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($typeOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_1)
    $il.Emit([Reflection.Emit.OpCodes]::Castclass, $InfoCall.GetType())
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $infoMethod)
    $il.Emit([Reflection.Emit.OpCodes]::Brtrue, $infoOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($infoOk)

    # Preserve the observable scratch copy, even when a later check refuses.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]52)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $copy)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $u32)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]0x400)
    $il.Emit([Reflection.Emit.OpCodes]::And)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $notReparse)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_3)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($notReparse)

    # A bound Expected must be a byte[52], after the native/reparse checks.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]5)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [byte[]])
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $identityFailure)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Ldlen)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_I4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]52)
    $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $identityFailure)

    # Unconditional spans: attributes u32@0, volume u32@28, index u64@44.
    # Content additionally compares size u64@32, write u64@20, links u32@40.
    # Every load is inside the same fixed 52-byte records; equality is bitwise.
    $fields = @(@(0, $u32), @(28, $u32), @(44, $u64),
                @(32, $u64), @(20, $u64), @(40, $u32))
    for ($i = 0; $i -lt $fields.Length; $i++) {
        if ($i -eq 3) {
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]6)
            $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $equal)
        }
        $offset = [int]$fields[$i][0]; $read = $fields[$i][1]
        $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
        $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, $offset)
        $il.Emit([Reflection.Emit.OpCodes]::Call, $read)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
        $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, $offset)
        $il.Emit([Reflection.Emit.OpCodes]::Call, $read)
        $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $changed)
    }
    $il.MarkLabel($equal)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($identityFailure)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($changed)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_4)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    return $method.CreateDelegate([Func[Delegate, Delegate, IntPtr, IntPtr, byte[], object, bool, int]])
}

# Complete unbound initial identity read/decoding, not a cached native fact.
# Return a fresh literal-equivalent six-field Hashtable, or boxed closed status:
# 1=type,2=identity,3=reparse,4=unsupported Invoke metadata (old path, no read yet).
function New-HostInitialInvoker([Delegate] $TypeCall, [Delegate] $InfoCall) {
    $typeMethod = $TypeCall.GetType().GetMethod('Invoke')
    $infoMethod = $InfoCall.GetType().GetMethod('Invoke')
    $method = [Reflection.Emit.DynamicMethod]::new('ReadInitialHostIdentity', [object],
        [Type[]]@([Delegate], [Delegate], [IntPtr], [IntPtr], [byte[]]))
    $il = $method.GetILGenerator(); $typeOk = $il.DefineLabel(); $infoOk = $il.DefineLabel()
    $clean = $il.DefineLabel(); $unsupported = $il.DefineLabel()
    $attributes = $il.DeclareLocal([uint32]); $result = $il.DeclareLocal([Collections.Hashtable])
    $asPSObject = [Management.Automation.PSObject].GetMethod('AsPSObject', [Type[]]@([object]))
    $getMembers = [Management.Automation.PSObject].GetMethod('get_Members', [Type[]]@())
    $collection = [Management.Automation.PSMemberInfoCollection[Management.Automation.PSMemberInfo]]
    $getMember = $collection.GetMethod('get_Item', [Type[]]@([string]))
    # Preserve same-object ETS Invoke probes too: no native call precedes fallback.
    foreach ($argument in @(0, 1)) {
        $il.Emit([Reflection.Emit.OpCodes]::Ldarg, [short]$argument)
        $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, 'Invoke')
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
        $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Management.Automation.PSMethod])
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $unsupported)
    }
    $copy = [Runtime.InteropServices.Marshal].GetMethod('Copy', [Type[]]@([IntPtr], [byte[]], [int], [int]))
    $u32 = [BitConverter].GetMethod('ToUInt32', [Type[]]@([byte[]], [int]))
    $hex = [Convert].GetMethod('ToHexString', [Type[]]@([byte[]], [int], [int]))
    $concat = [string].GetMethod('Concat', [Type[]]@([string], [string]))
    $comparer = [StringComparer].GetMethod('get_OrdinalIgnoreCase', [Type[]]@())
    $ctor = [Collections.Hashtable].GetConstructor([Type[]]@([int], [Collections.IEqualityComparer]))
    $add = [Collections.Hashtable].GetMethod('Add', [Type[]]@([object], [object]))
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Castclass, $TypeCall.GetType())
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $typeMethod)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Beq, $typeOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Box, [int])
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($typeOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_1)
    $il.Emit([Reflection.Emit.OpCodes]::Castclass, $InfoCall.GetType())
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $infoMethod)
    $il.Emit([Reflection.Emit.OpCodes]::Brtrue, $infoOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_2)
    $il.Emit([Reflection.Emit.OpCodes]::Box, [int])
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($infoOk)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]52)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $copy)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $u32)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $attributes)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $attributes)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]0x400)
    $il.Emit([Reflection.Emit.OpCodes]::And)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $clean)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_3)
    $il.Emit([Reflection.Emit.OpCodes]::Box, [int])
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($clean)
    # Exactly PowerShell's six-entry literal capacity and OrdinalIgnoreCase comparer.
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_6)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $comparer)
    $il.Emit([Reflection.Emit.OpCodes]::Newobj, $ctor)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $result)
    foreach ($field in @('attributes', 'directory', 'bytes', 'write', 'links', 'identity')) {
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $result)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $field)
        switch ($field) {
            'attributes' {
                $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $attributes)
                $il.Emit([Reflection.Emit.OpCodes]::Box, [uint32]); break
            }
            'directory' {
                $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $attributes)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]0x10)
                $il.Emit([Reflection.Emit.OpCodes]::And)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
                $il.Emit([Reflection.Emit.OpCodes]::Cgt_Un)
                $il.Emit([Reflection.Emit.OpCodes]::Box, [bool]); break
            }
            'bytes' {
                $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]32)
                $il.Emit([Reflection.Emit.OpCodes]::Call, $u32)
                $il.Emit([Reflection.Emit.OpCodes]::Conv_U8)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I8, [long]4294967296)
                $il.Emit([Reflection.Emit.OpCodes]::Mul)
                $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]36)
                $il.Emit([Reflection.Emit.OpCodes]::Call, $u32)
                $il.Emit([Reflection.Emit.OpCodes]::Conv_U8)
                $il.Emit([Reflection.Emit.OpCodes]::Add)
                $il.Emit([Reflection.Emit.OpCodes]::Box, [uint64]); break
            }
            'write' {
                $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]20)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_8)
                $il.Emit([Reflection.Emit.OpCodes]::Call, $hex); break
            }
            'links' {
                $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
                $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]40)
                $il.Emit([Reflection.Emit.OpCodes]::Call, $u32)
                $il.Emit([Reflection.Emit.OpCodes]::Box, [uint32]); break
            }
            'identity' {
                foreach ($span in @(@(28, 4), @(44, 8))) {
                    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
                    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$span[0])
                    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$span[1])
                    $il.Emit([Reflection.Emit.OpCodes]::Call, $hex)
                }
                $il.Emit([Reflection.Emit.OpCodes]::Call, $concat); break
            }
        }
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $add)
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $result)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($unsupported)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_4)
    $il.Emit([Reflection.Emit.OpCodes]::Box, [int])
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    return $method.CreateDelegate([Func[Delegate, Delegate, IntPtr, IntPtr, byte[], object]])
}

# Collapse the cached root-to-leaf comparison loop into the managed runtime.
# Each entry is read live, never a retained native fact or handle/Expected copy.
# 0=all checked; 1..4=the existing comparison status; -(index+1)=resume the
# original PowerShell path at this entry, with no completed check repeated.
function New-HostChainInvoker([Delegate] $ComparisonCall) {
    $comparisonMethod = $ComparisonCall.GetType().GetMethod('Invoke')
    $method = [Reflection.Emit.DynamicMethod]::new('CheckHostDirectoryChain', [int],
        [Type[]]@([object[]], [Delegate], [Delegate], [Delegate], [IntPtr], [byte[]]))
    $il = $method.GetILGenerator()
    $index = $il.DeclareLocal([int])
    $record = $il.DeclareLocal([Collections.Hashtable])
    $handle = $il.DeclareLocal([object])
    $expected = $il.DeclareLocal([object])
    $membersType = [Management.Automation.PSMemberInfoCollection[Management.Automation.PSMemberInfo]]
    $members = $il.DeclareLocal($membersType)
    $status = $il.DeclareLocal([int])
    $test = $il.DefineLabel(); $next = $il.DefineLabel(); $advance = $il.DefineLabel(); $fallback = $il.DefineLabel()
    $getType = [object].GetMethod('GetType', [Type[]]@())
    $typeFromHandle = [Type].GetMethod('GetTypeFromHandle', [Type[]]@([RuntimeTypeHandle]))
    $contains = [Collections.Hashtable].GetMethod('ContainsKey', [Type[]]@([object]))
    $item = [Collections.Hashtable].GetMethod('get_Item', [Type[]]@([object]))
    $asPSObject = [Management.Automation.PSObject].GetMethod('AsPSObject', [Type[]]@([object]))
    $getMembers = [Management.Automation.PSObject].GetMethod('get_Members', [Type[]]@())
    $getMember = $membersType.GetMethod('get_Item', [Type[]]@([string]))
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $index)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldlen)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_I4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]512)
    $il.Emit([Reflection.Emit.OpCodes]::Bgt, $fallback)
    $il.Emit([Reflection.Emit.OpCodes]::Br, $test)
    $il.MarkLabel($next)
    # Only the exact internally-created Hashtable shape bypasses adaptation.
    # Subclasses, missing members and convertible/non-IntPtr handles resume
    # original property access and parameter binding before any native read.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $index)
    $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Collections.Hashtable])
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $fallback)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getType)
    $il.Emit([Reflection.Emit.OpCodes]::Ldtoken, [Collections.Hashtable])
    $il.Emit([Reflection.Emit.OpCodes]::Call, $typeFromHandle)
    $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $fallback)
    # ETS instance members precede dictionary keys in PowerShell. Inspect
    # public member metadata only, never Value/getters. Any shadow resumes
    # original adaptation before this entry's property evaluation/native read.
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $members)
    foreach ($key in @('handle', 'comparison')) {
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $members)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
        $il.Emit([Reflection.Emit.OpCodes]::Brtrue, $fallback)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $contains)
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $fallback)
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Ldstr, 'handle')
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $item)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $handle)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $handle)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [IntPtr])
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $fallback)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $record)
    $il.Emit([Reflection.Emit.OpCodes]::Ldstr, 'comparison')
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $item)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [byte[]])
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $fallback)
    # Invoke the exact owned comparison: Type -> Info -> scratch Copy ->
    # reparse -> current Expected shape -> identity fields, Content=false.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_1)
    $il.Emit([Reflection.Emit.OpCodes]::Castclass, $ComparisonCall.GetType())
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $handle)
    $il.Emit([Reflection.Emit.OpCodes]::Unbox_Any, [IntPtr])
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]5)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $comparisonMethod)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $status)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $status)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $advance)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $status)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($advance)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $index)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Add)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $index)
    $il.MarkLabel($test)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $index)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldlen)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_I4)
    $il.Emit([Reflection.Emit.OpCodes]::Blt, $next)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($fallback)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $index)
    $il.Emit([Reflection.Emit.OpCodes]::Not)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    return $method.CreateDelegate([Func[object[], Delegate, Delegate, Delegate, IntPtr, byte[], int]])
}

# Evaluate the COMPLETE existing per-file hash admission in managed code.
# This is pure owner/type/ETS metadata work: no Value/getter, FS or native read.
# Unsupported/probed state returns false and uses the original admission below.
function New-HostHashAdmissionInvoker {
    $method = [Reflection.Emit.DynamicMethod]::new('AdmitHostHashOwners', [bool], [Type[]]@([object[]]))
    $il = $method.GetILGenerator(); $no = $il.DefineLabel(); $textDone = $il.DefineLabel()
    $limits = $il.DeclareLocal([Collections.Hashtable]); $value = $il.DeclareLocal([object])
    $zeroSpan = $il.DeclareLocal([TimeSpan])
    $asPSObject = [Management.Automation.PSObject].GetMethod('AsPSObject', [Type[]]@([object]))
    $getMembers = [Management.Automation.PSObject].GetMethod('get_Members', [Type[]]@())
    $collection = [Management.Automation.PSMemberInfoCollection[Management.Automation.PSMemberInfo]]
    $limitMembers = $il.DeclareLocal($collection)
    $getMember = $collection.GetMethod('get_Item', [Type[]]@([string]))
    $item = [Collections.Hashtable].GetMethod('get_Item', [Type[]]@([object]))
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldlen)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_I4)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]20)
    $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $no)
    # Same invoker and three function owners; exact limits and handles owners.
    foreach ($pair in @(@(0, 1), @(2, 3), @(4, 5), @(6, 7), @(8, 9), @(10, 11))) {
        foreach ($offset in $pair) {
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
            $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$offset)
            $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
        }
        $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $no)
    }
    foreach ($typed in @(@(0, [Delegate]), @(12, [Diagnostics.Stopwatch]), @(13, [byte[]]),
        @(10, [Collections.Generic.List[IntPtr]]), @(14, [int]), @(15, [uint64]), @(16, [int]))) {
        $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
        $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$typed[0])
        $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
        $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Type]$typed[1])
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_8)
    $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Collections.Hashtable])
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $limits)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $limits)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $limits)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $limitMembers)
    foreach ($key in @('Seconds', 'Entries', 'TotalBytes', 'Handles', 'MetadataBytes')) {
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $limitMembers)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
        $il.Emit([Reflection.Emit.OpCodes]::Brtrue, $no)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $limits)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $item)
        if ($key -ceq 'TotalBytes') {
            $il.Emit([Reflection.Emit.OpCodes]::Stloc, $value)
            $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $value)
            $il.Emit([Reflection.Emit.OpCodes]::Isinst, [long])
            $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
            $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $value)
            $il.Emit([Reflection.Emit.OpCodes]::Unbox_Any, [long])
            $il.Emit([Reflection.Emit.OpCodes]::Ldc_I8, [long]0)
            $il.Emit([Reflection.Emit.OpCodes]::Blt, $no)
        } else {
            $il.Emit([Reflection.Emit.OpCodes]::Isinst, [int])
            $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
        }
    }
    # Metadata-only ordinary properties/methods, including global TypeData.
    $checks = @(@(12, 'Elapsed', [Management.Automation.PSProperty]),
        @(-1, 'TotalSeconds', [Management.Automation.PSProperty]),
        @(13, 'Length', [Management.Automation.PSProperty]),
        @(10, 'Count', [Management.Automation.PSProperty]),
        @(17, 'Read', [Management.Automation.PSMethod]),
        @(18, 'TransformBlock', [Management.Automation.PSMethod]),
        @(18, 'TransformFinalBlock', [Management.Automation.PSMethod]))
    foreach ($check in $checks) {
        if ($check[0] -eq -1) {
            $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $zeroSpan)
            $il.Emit([Reflection.Emit.OpCodes]::Box, [TimeSpan])
        } else {
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
            $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$check[0])
            $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
        }
        $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, [string]$check[1])
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
        $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Type]$check[2])
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]19)
    $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $textDone)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]19)
    $il.Emit([Reflection.Emit.OpCodes]::Ldelem_Ref)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
    $il.Emit([Reflection.Emit.OpCodes]::Ldstr, 'Write')
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Management.Automation.PSMethod])
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $no)
    $il.MarkLabel($textDone)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_1)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($no)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    return $method.CreateDelegate([Func[object[], bool]])
}

# Own the complete synchronous 1MiB payload-read/hash loop in managed code.
# 0=complete, 1..5=deadline/entries/bytes/handles/metadata, 6=changed, 7=adaptation.
# Native open/ancestor/final identity checks remain at their original callers.
function New-HostHashInvoker {
    $method = [Reflection.Emit.DynamicMethod]::new('ReadAndHashHostFile', [int],
        [Type[]]@([IO.Stream], [Security.Cryptography.HashAlgorithm], [byte[]], [IO.MemoryStream],
            [object], [Diagnostics.Stopwatch], [long[]], [uint64], [uint64], [uint64[]]))
    $il = $method.GetILGenerator()
    $size = $il.DeclareLocal([uint64]); $count = $il.DeclareLocal([int])
    $record = $il.DeclareLocal([Collections.Hashtable])
    $initial = $il.DeclareLocal([Collections.Hashtable])
    $expected = $il.DeclareLocal([uint64])
    $boxedBytes = $il.DeclareLocal([object])
    $elapsed = $il.DeclareLocal([TimeSpan])
    $read = $il.DefineLabel(); $body = $il.DefineLabel()
    $changed = $il.DefineLabel(); $afterText = $il.DefineLabel(); $adapted = $il.DefineLabel()
    $refused = @{}
    foreach ($code in @(1, 2, 3, 4, 5)) { $refused[$code] = $il.DefineLabel() }
    $streamRead = [IO.Stream].GetMethod('Read', [Type[]]@([byte[]], [int], [int]))
    $transform = [Security.Cryptography.HashAlgorithm].GetMethod('TransformBlock',
        [Type[]]@([byte[]], [int], [int], [byte[]], [int]))
    $finalize = [Security.Cryptography.HashAlgorithm].GetMethod('TransformFinalBlock',
        [Type[]]@([byte[]], [int], [int]))
    $write = [IO.MemoryStream].GetMethod('Write', [Type[]]@([byte[]], [int], [int]))
    $getElapsed = [Diagnostics.Stopwatch].GetMethod('get_Elapsed', [Type[]]@())
    $seconds = [TimeSpan].GetMethod('get_TotalSeconds', [Type[]]@())
    $getType = [object].GetMethod('GetType', [Type[]]@())
    $typeFromHandle = [Type].GetMethod('GetTypeFromHandle', [Type[]]@([RuntimeTypeHandle]))
    $asPSObject = [Management.Automation.PSObject].GetMethod('AsPSObject', [Type[]]@([object]))
    $getMembers = [Management.Automation.PSObject].GetMethod('get_Members', [Type[]]@())
    $memberCollection = [Management.Automation.PSMemberInfoCollection[Management.Automation.PSMemberInfo]]
    $getMember = $memberCollection.GetMethod('get_Item', [Type[]]@([string]))
    $contains = [Collections.Hashtable].GetMethod('ContainsKey', [Type[]]@([object]))
    $item = [Collections.Hashtable].GetMethod('get_Item', [Type[]]@([object]))
    # Admit only exact ordinary Hashtable -> Hashtable -> UInt64 facts. No
    # initial/bytes property Value or conversion runs before the original Read.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]4)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Collections.Hashtable])
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $record)
    foreach ($field in @(@($record, 'initial', $initial), @($initial, 'bytes', $boxedBytes))) {
        $owner = $field[0]; $key = [string]$field[1]; $destination = $field[2]
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $owner)
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $adapted)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $owner)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getType)
        $il.Emit([Reflection.Emit.OpCodes]::Ldtoken, [Collections.Hashtable])
        $il.Emit([Reflection.Emit.OpCodes]::Call, $typeFromHandle)
        $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $adapted)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $owner)
        $il.Emit([Reflection.Emit.OpCodes]::Call, $asPSObject)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMembers)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getMember)
        $il.Emit([Reflection.Emit.OpCodes]::Brtrue, $adapted)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $owner)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $contains)
        $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $adapted)
        $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $owner)
        $il.Emit([Reflection.Emit.OpCodes]::Ldstr, $key)
        $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $item)
        if ($key -ceq 'initial') { $il.Emit([Reflection.Emit.OpCodes]::Isinst, [Collections.Hashtable]) }
        $il.Emit([Reflection.Emit.OpCodes]::Stloc, $destination)
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $boxedBytes)
    $il.Emit([Reflection.Emit.OpCodes]::Isinst, [uint64])
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $adapted)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $boxedBytes)
    $il.Emit([Reflection.Emit.OpCodes]::Unbox_Any, [uint64])
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $expected)
    # Fixed CLR type inspection cannot execute a PowerShell GetType shadow.
    # A derived/adapted clock resumes the old loop before any Read/hash call.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]5)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getType)
    $il.Emit([Reflection.Emit.OpCodes]::Ldtoken, [Diagnostics.Stopwatch])
    $il.Emit([Reflection.Emit.OpCodes]::Call, $typeFromHandle)
    $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $adapted)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_U8)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Br, $read)
    $il.MarkLabel($body)
    # Exact Assert-HostBudget predicate order after each positive Read.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]5)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $getElapsed)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $elapsed)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloca, $elapsed)
    $il.Emit([Reflection.Emit.OpCodes]::Call, $seconds)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]6)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldelem_I8)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_R8)
    $il.Emit([Reflection.Emit.OpCodes]::Bgt, $refused[1])
    # State: seconds,current/max entries,current/max handles,current/max metadata.
    foreach ($check in @(@(1, 2, 2), @(3, 4, 4), @(5, 6, 5))) {
        if ($check[2] -eq 4) {
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]7)
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]8)
            $il.Emit([Reflection.Emit.OpCodes]::Bgt_Un, $refused[3])
        }
        foreach ($offset in @($check[0], $check[1])) {
            $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]6)
            $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$offset)
            $il.Emit([Reflection.Emit.OpCodes]::Ldelem_I8)
        }
        $il.Emit([Reflection.Emit.OpCodes]::Bgt, $refused[[int]$check[2]])
    }
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $count)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_U8)
    $il.Emit([Reflection.Emit.OpCodes]::Add)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Bgt_Un, $changed)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_1)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $count)
    $il.Emit([Reflection.Emit.OpCodes]::Ldnull)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $transform)
    $il.Emit([Reflection.Emit.OpCodes]::Pop)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Brfalse, $afterText)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_3)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $count)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $write)
    $il.MarkLabel($afterText)
    $il.MarkLabel($read)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_2)
    $il.Emit([Reflection.Emit.OpCodes]::Ldlen)
    $il.Emit([Reflection.Emit.OpCodes]::Conv_I4)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $streamRead)
    $il.Emit([Reflection.Emit.OpCodes]::Stloc, $count)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $count)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Bgt, $body)
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_1)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Newarr, [byte])
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Callvirt, $finalize)
    $il.Emit([Reflection.Emit.OpCodes]::Pop)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $expected)
    $il.Emit([Reflection.Emit.OpCodes]::Bne_Un, $changed)
    # Return the accumulated size through one fresh private array element,
    # avoiding a second PowerShell initial.bytes lookup after successful EOF.
    $il.Emit([Reflection.Emit.OpCodes]::Ldarg_S, [byte]9)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ldloc, $size)
    $il.Emit([Reflection.Emit.OpCodes]::Stelem_I8)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_0)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    foreach ($code in @(1, 2, 3, 4, 5)) {
        $il.MarkLabel($refused[$code])
        $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4, [int]$code)
        $il.Emit([Reflection.Emit.OpCodes]::Ret)
    }
    $il.MarkLabel($changed)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_6)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    $il.MarkLabel($adapted)
    $il.Emit([Reflection.Emit.OpCodes]::Ldc_I4_7)
    $il.Emit([Reflection.Emit.OpCodes]::Ret)
    return $method.CreateDelegate([Func[IO.Stream, Security.Cryptography.HashAlgorithm, byte[], IO.MemoryStream,
        object, Diagnostics.Stopwatch, long[], uint64, uint64, uint64[], int]])
}

# Compile only fixed source expressions in the existing managed runtime. No
# compiler executable, native export, file fact, or caller-supplied expression
# is used. Unicode inputs return -1 for the unchanged PowerShell validator:
# this preserves its Regex cache/culture and PowerShell comparison semantics.
function New-HostCanonicalInvoker {
    $e = [Linq.Expressions.Expression]
    # Select exact public signatures once. PowerShell must not choose among
    # collection/params overloads or Return's Type/Expression alternatives.
    # These delegates build only the fixed graph below; no input is compiled.
    $expressions = [Collections.Generic.IEnumerable[Linq.Expressions.Expression]]
    $parameters = [Collections.Generic.IEnumerable[Linq.Expressions.ParameterExpression]]
    $new = $e.GetMethod('New', [Type[]]@([Reflection.ConstructorInfo], $expressions)).CreateDelegate(
        [Func[Reflection.ConstructorInfo, Collections.Generic.IEnumerable[Linq.Expressions.Expression], Linq.Expressions.NewExpression]])
    $returnValue = $e.GetMethod('Return', [Type[]]@([Linq.Expressions.LabelTarget], [Linq.Expressions.Expression])).CreateDelegate(
        [Func[Linq.Expressions.LabelTarget, Linq.Expressions.Expression, Linq.Expressions.GotoExpression]])
    $staticCall = $e.GetMethod('Call', [Type[]]@([Reflection.MethodInfo], $expressions)).CreateDelegate(
        [Func[Reflection.MethodInfo, Collections.Generic.IEnumerable[Linq.Expressions.Expression], Linq.Expressions.MethodCallExpression]])
    $instanceCall = $e.GetMethod('Call', [Type[]]@([Linq.Expressions.Expression], [Reflection.MethodInfo], $expressions)).CreateDelegate(
        [Func[Linq.Expressions.Expression, Reflection.MethodInfo, Collections.Generic.IEnumerable[Linq.Expressions.Expression], Linq.Expressions.MethodCallExpression]])
    $block = $e.GetMethod('Block', [Type[]]@($expressions)).CreateDelegate(
        [Func[Collections.Generic.IEnumerable[Linq.Expressions.Expression], Linq.Expressions.BlockExpression]])
    $variableBlock = $e.GetMethod('Block', [Type[]]@($parameters, $expressions)).CreateDelegate(
        [Func[Collections.Generic.IEnumerable[Linq.Expressions.ParameterExpression], Collections.Generic.IEnumerable[Linq.Expressions.Expression], Linq.Expressions.BlockExpression]])
    $lambda = $e.GetMethod('Lambda', [Type[]]@([Type], [Linq.Expressions.Expression], $parameters)).CreateDelegate(
        [Func[Type, Linq.Expressions.Expression, Collections.Generic.IEnumerable[Linq.Expressions.ParameterExpression], Linq.Expressions.LambdaExpression]])
    $resultType = [ValueTuple[string, int]]
    $resultCtor = $resultType.GetConstructor([Type[]]@([string], [int]))
    $value = $e::Parameter([string], 'value')
    $full = $e::Variable([string], 'full')
    $parts = $e::Variable([string[]], 'parts')
    $part = $e::Variable([string], 'part')
    $index = $e::Variable([int], 'index')
    $finish = $e::Label($resultType, 'finish')
    $asciiDone = $e::Label('ascii_done'); $partsDone = $e::Label('parts_done')
    $zero = $e::Constant([int]0)
    $nullString = $e::Constant($null, [string])
    $refused = @{}
    foreach ($code in @(-1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13)) {
        $refused[$code] = $returnValue.Invoke($finish,
            $new.Invoke($resultCtor, [Linq.Expressions.Expression[]]@($nullString, $e::Constant([int]$code))))
    }
    $empty = [string].GetMethod('IsNullOrEmpty', [Type[]]@([string]))
    $charAt = [string].GetMethod('get_Chars', [Type[]]@([int]))
    $substring = [string].GetMethod('Substring', [Type[]]@([int]))
    $contains = [string].GetMethod('Contains', [Type[]]@([string]))
    $endsWith = [string].GetMethod('EndsWith', [Type[]]@([string]))
    $trimEnd = [string].GetMethod('TrimEnd', [Type[]]@([char[]]))
    $split = [string].GetMethod('Split', [Type[]]@([char[]], [StringSplitOptions]))
    $getFullPath = [IO.Path].GetMethod('GetFullPath', [Type[]]@([string]))
    # These are the public PowerShell equality semantics, not ordinal equality.
    $psEquals = [Management.Automation.LanguagePrimitives].GetMethod('Equals',
        [Type[]]@([object], [object], [bool]))
    $slash = $e::Constant([char[]]@([char]92), [char[]])
    $ignoreCase = $e::Constant($true); $caseSensitive = $e::Constant($false)
    $regexOptions = [Text.RegularExpressions.RegexOptions]::IgnoreCase
    $drive = [Text.RegularExpressions.Regex]::new('^[A-Za-z]:\\', $regexOptions)
    $invalid = [Text.RegularExpressions.Regex]::new('[\x00-\x1f*?"<>|]', $regexOptions)
    $device = [Text.RegularExpressions.Regex]::new('^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)', $regexOptions)
    $isMatch = [Text.RegularExpressions.Regex].GetMethod('IsMatch', [Type[]]@([string]))
    $length = $e::Property($value, 'Length')
    $partObject = $e::Convert($part, [object])
    $partEmpty = $staticCall.Invoke($psEquals, [Linq.Expressions.Expression[]]@(
        $partObject, $e::Constant('', [object]), $ignoreCase))
    $dot = $staticCall.Invoke($psEquals, [Linq.Expressions.Expression[]]@(
        $partObject, $e::Constant('.', [object]), $ignoreCase))
    $dotDot = $staticCall.Invoke($psEquals, [Linq.Expressions.Expression[]]@(
        $partObject, $e::Constant('..', [object]), $ignoreCase))
    $git = $staticCall.Invoke($psEquals, [Linq.Expressions.Expression[]]@(
        $partObject, $e::Constant('.git', [object]), $ignoreCase))
    $trimmedFull = $instanceCall.Invoke($full, $trimEnd, [Linq.Expressions.Expression[]]@($slash))
    $trimmedValue = $instanceCall.Invoke($value, $trimEnd, [Linq.Expressions.Expression[]]@($slash))
    $normalized = $staticCall.Invoke($psEquals, [Linq.Expressions.Expression[]]@(
        $e::Convert($trimmedFull, [object]), $e::Convert($trimmedValue, [object]), $caseSensitive))
    $componentChecks = $block.Invoke([Linq.Expressions.Expression[]]@(
        $e::IfThen($partEmpty, $refused[8]),
        $e::IfThen($e::OrElse($dot, $dotDot), $refused[9]),
        $e::IfThen($instanceCall.Invoke($part, $endsWith, [Linq.Expressions.Expression[]]@($e::Constant('.'))), $refused[10]),
        $e::IfThen($instanceCall.Invoke($part, $endsWith, [Linq.Expressions.Expression[]]@($e::Constant(' '))), $refused[11]),
        $e::IfThen($instanceCall.Invoke($e::Constant($device), $isMatch, [Linq.Expressions.Expression[]]@($part)), $refused[12]),
        $e::IfThen($git, $refused[13]), $e::Empty()))
    $asciiLoop = $e::Loop($e::IfThenElse($e::LessThan($index, $length),
        $block.Invoke([Linq.Expressions.Expression[]]@(
            $e::IfThen($e::GreaterThan($e::Convert(
                $instanceCall.Invoke($value, $charAt, [Linq.Expressions.Expression[]]@($index)), [int]), $e::Constant([int]127)), $refused[-1]),
            $e::PostIncrementAssign($index), $e::Empty())), $e::Break($asciiDone)), $asciiDone)
    $componentLoop = $e::Loop($e::IfThenElse($e::LessThan($index, $e::ArrayLength($parts)),
        $block.Invoke([Linq.Expressions.Expression[]]@(
            $e::Assign($part, $e::ArrayIndex($parts, $index)),
            $e::IfThen($e::Not($e::AndAlso($partEmpty, $e::Equal($length, $e::Constant([int]3)))), $componentChecks),
            $e::PostIncrementAssign($index), $e::Empty())), $e::Break($partsDone)), $partsDone)
    $body = $variableBlock.Invoke([Linq.Expressions.ParameterExpression[]]@($full, $parts, $part, $index),
        [Linq.Expressions.Expression[]]@(
            $e::IfThen($staticCall.Invoke($empty, [Linq.Expressions.Expression[]]@($value)), $refused[1]),
            $e::IfThen($e::GreaterThan($length, $e::Constant([int]1024)), $refused[2]),
            $e::Assign($index, $zero), $asciiLoop,
            $e::IfThen($e::Not($instanceCall.Invoke($e::Constant($drive), $isMatch, [Linq.Expressions.Expression[]]@($value))), $refused[3]),
            $e::IfThen($instanceCall.Invoke($instanceCall.Invoke($value, $substring, [Linq.Expressions.Expression[]]@($e::Constant([int]2))),
                $contains, [Linq.Expressions.Expression[]]@($e::Constant(':'))), $refused[4]),
            $e::IfThen($instanceCall.Invoke($value, $contains, [Linq.Expressions.Expression[]]@($e::Constant('/'))), $refused[5]),
            $e::IfThen($instanceCall.Invoke($e::Constant($invalid), $isMatch, [Linq.Expressions.Expression[]]@($value)), $refused[6]),
            $e::Assign($full, $staticCall.Invoke($getFullPath, [Linq.Expressions.Expression[]]@($value))),
            $e::IfThen($e::Not($normalized), $refused[7]),
            $e::Assign($parts, $instanceCall.Invoke($instanceCall.Invoke($value, $substring, [Linq.Expressions.Expression[]]@($e::Constant([int]3))),
                $split, [Linq.Expressions.Expression[]]@($slash, $e::Constant([StringSplitOptions]::None)))),
            $e::Assign($index, $zero), $componentLoop,
            $e::Label($finish, $new.Invoke($resultCtor, [Linq.Expressions.Expression[]]@($full, $zero)))))
    return $lambda.Invoke([Func[string, ValueTuple[string, int]]], $body,
        [Linq.Expressions.ParameterExpression[]]@($value)).Compile()
}

# New explicit setup boundary: compile ONLY this fixed embedded source with
# PowerShell's bundled in-process Roslyn/default reference assemblies. No
# external compiler, SDK program, package, source file or output assembly runs.
# This capability requires separate source/compiler admission before execution.
function Get-HostOwnedTraversalType {
    $source = @'
// Fixed source compiled in-process by PowerShell's bundled Roslyn, only after
// separate compiler/setup admission. No P/Invoke declaration, external process,
// package, assembly download, parallel walk, or reusable filesystem fact cache.
using System;
using System.Collections;
using System.Collections.Generic;
using System.Collections.Specialized;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Linq.Expressions;
using System.Management.Automation;
using System.Management.Automation.Runspaces;
using System.Runtime.InteropServices;
using System.Runtime.CompilerServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using Microsoft.Win32.SafeHandles;

namespace Morpheus.OwnedTraversal20261028
{
    public sealed class Reader
    {
        private static readonly ConditionalWeakTable<List<IntPtr>, Reader> ownedReaders = new ConditionalWeakTable<List<IntPtr>, Reader>();
        private sealed class Ownership { public bool Closed; public IntPtr Memory, Kernel; }
        private static readonly ConditionalWeakTable<List<IntPtr>, Ownership> owners = new ConditionalWeakTable<List<IntPtr>, Ownership>();
        private static readonly object ownerLock = new object();

        // Initialization registers every owner, including a reader for which
        // managed admission is never attempted or is refused. Close latches
        // this private state even when no Reader instance was constructed.
        public static void PrepareOwner(List<IntPtr> handles)
        {
            lock (ownerLock)
            {
                if (owners.TryGetValue(handles, out Ownership existing)) Fail("identity");
                owners.Add(handles, new Ownership());
            }
        }
        public static void PublishOwner(List<IntPtr> handles, IntPtr memory, IntPtr kernel)
        {
            lock (ownerLock)
            {
                if (!owners.TryGetValue(handles, out Ownership owner) || owner.Closed ||
                    owner.Memory != IntPtr.Zero || memory == IntPtr.Zero || kernel == IntPtr.Zero) Fail("identity");
                owner.Memory = memory; owner.Kernel = kernel;
            }
        }
        // These objects remain owned by Initialize/Close-HostReader. In
        // particular, SafeFileHandle wrappers below NEVER own the native handle.
        public readonly List<IntPtr> Handles;
        public readonly Dictionary<string, object> Directories;
        public readonly List<object> FileRecords;
        public readonly Dictionary<string, string> Memo;
        public readonly Stopwatch Watch;
        public readonly byte[] Scratch, Buffer;
        public IntPtr Memory { get; private set; }
        public readonly Hashtable Limits;
        public int Entries, Metadata, MemoUnits, CompletedFiles, InitialReads, CompletedRoots;
        public ulong Bytes;
        public string Stage = "initialization", Work = "idle", Cause = "unknown";
        private bool busy, entered, active, retired;
        private string lastPath;
        private Record[] lastChain;
        private readonly Dictionary<string, Record> directoryRecords = new Dictionary<string, Record>(StringComparer.OrdinalIgnoreCase);
        private readonly List<Record> fileRecords = new List<Record>();
        private readonly Func<IntPtr, uint, uint, IntPtr, uint, uint, IntPtr, IntPtr> open;
        private readonly Func<IntPtr, uint> type;
        private readonly Func<IntPtr, IntPtr, int> info;
        private readonly ScriptBlock unicodeValidator;
        // One private provider per entered sequential transaction. Successful
        // TransformFinalBlock already resets its reusable CNG hash state.
        private SHA256 payloadDigest;

        private sealed class Record
        {
            public string Path;
            public IntPtr Handle;
            public Hashtable Initial, Public;
            public byte[] Comparison;
        }
        private sealed class Pending { public string Path; public int Depth; }
        private sealed class Snapshot { public Record Directory; public string[] Names; }

        // A closed expression binds the already-owned native delegate's exact
        // Invoke signature once. It introduces neither an export nor new ABI.
        private static T Bind<T>(Delegate native) where T : Delegate
        {
            var signature = typeof(T).GetMethod("Invoke");
            var parameters = signature.GetParameters();
            var args = new ParameterExpression[parameters.Length];
            for (int i = 0; i < args.Length; i++) args[i] = Expression.Parameter(parameters[i].ParameterType);
            return Expression.Lambda<T>(Expression.Invoke(Expression.Constant(native, native.GetType()), args), args).Compile();
        }

        private Reader(List<IntPtr> handles, Dictionary<string, object> directories,
            List<object> records, Dictionary<string, string> memo, int units,
            Stopwatch watch, byte[] scratch, byte[] buffer, IntPtr memory,
            Delegate openCall, Delegate typeCall, Delegate infoCall, Hashtable limits, ScriptBlock unicodeSource)
        {
            Handles = handles; Directories = directories; FileRecords = records;
            Memo = memo; MemoUnits = units; Watch = watch; Scratch = scratch;
            Buffer = buffer; Memory = memory; Limits = limits; unicodeValidator = unicodeSource;
            open = Bind<Func<IntPtr, uint, uint, IntPtr, uint, uint, IntPtr, IntPtr>>(openCall);
            type = Bind<Func<IntPtr, uint>>(typeCall);
            info = Bind<Func<IntPtr, IntPtr, int>>(infoCall);
        }

        private static bool Property(object value, string name)
        { return PSObject.AsPSObject(value).Members[name] is PSProperty; }
        private static bool Method(object value, string name)
        { return PSObject.AsPSObject(value).Members[name] is PSMethod; }
        private static bool NoMember(object value, string name)
        { return PSObject.AsPSObject(value).Members[name] == null; }

        // Catalog inspection is metadata only: no PSMemberInfo.Value or getter
        // runs, and no SHA/stream is constructed to discover its runtime type.
        // Deny relevant extensions for every possible crypto implementation;
        // ordinary .NET inherited methods remain the original methods.
        private static bool Catalog(TypeData[] catalog)
        {
            if (catalog == null || catalog.Length > 1024) return false;
            var names = new HashSet<string>(StringComparer.OrdinalIgnoreCase) {
                "Read", "Write", "Dispose", "ToArray", "TransformBlock", "TransformFinalBlock", "Hash",
                "GetString", "Invoke", "GetType", "Length", "Count", "Elapsed", "TotalSeconds",
                "Contains", "Substring", "TrimEnd", "Split", "EndsWith", "Replace", "Trim", "ToLowerInvariant",
                "Add", "TryGetValue", "ContainsKey", "Push", "Pop", "Clear", "Clone", "Values", "Keys",
                "attributes", "directory", "bytes", "write", "links", "identity", "path", "handle",
                "initial", "comparison", "depth", "names", "root", "files", "sha256", "fact", "text",
                "Entries", "Metadata", "MemoUnits", "CompletedFiles", "InitialReads", "Stage", "Work", "Cause",
                "Retire", "RunRoot", "RunToolchain", "TryCreate"
            };
            foreach (TypeData data in catalog)
            {
                if (data == null || data.TypeName == null || data.TypeName.Length > 1024 || data.Members.Count > 1024) return false;
                string n = data.TypeName;
                if (n.StartsWith("Morpheus.OwnedTraversal20261028.", StringComparison.OrdinalIgnoreCase) && data.Members.Count != 0) return false;
                bool relevant = String.Equals(n, "System.Object", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.ValueType", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Enum", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("System.Security.Cryptography.", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("System.IO.", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("System.Text.", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("System.Collections.", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("Microsoft.Win32.SafeHandles.", StringComparison.OrdinalIgnoreCase) ||
                    String.Equals(n, "System.Runtime.InteropServices.SafeHandle", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Runtime.ConstrainedExecution.CriticalFinalizerObject", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("System.Diagnostics.Stopwatch", StringComparison.OrdinalIgnoreCase) ||
                    n.StartsWith("Morpheus.OwnedTraversal20261028.", StringComparison.OrdinalIgnoreCase) ||
                    String.Equals(n, "System.String", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Array", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Byte[]", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.TimeSpan", StringComparison.OrdinalIgnoreCase) ||
                    String.Equals(n, "System.Int32", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Int64", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.UInt32", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.UInt64", StringComparison.OrdinalIgnoreCase) ||
                    String.Equals(n, "System.Boolean", StringComparison.OrdinalIgnoreCase) || String.Equals(n, "System.Double", StringComparison.OrdinalIgnoreCase);
                if (!relevant) continue;
                if (data.TypeAdapter != null || data.TypeConverter != null) return false;
                foreach (KeyValuePair<string, TypeMemberData> member in data.Members)
                {
                    if (!names.Contains(member.Key)) continue;
                    // PowerShell's default array Count alias is benign: the
                    // original uses Count on one-dimensional names arrays.
                    if (String.Equals(n, "System.Array", StringComparison.OrdinalIgnoreCase) && member.Key.Equals("Count", StringComparison.OrdinalIgnoreCase) &&
                        member.Value is AliasPropertyData alias && alias.ReferencedMemberName == "Length" && alias.MemberType == null) continue;
                    return false;
                }
            }
            return true;
        }

        public static Reader TryCreate(object currentValue, object ownersValue, TypeData[] catalog,
            IntPtr memory, IntPtr memoryOwner, IntPtr kernel, IntPtr kernelOwner, bool scratchBusy, object entries, object bytes, object metadata,
            object units, object entryLimit, object unitLimit, object chainLimit, string systemRoot)
        {
            // Fixed raw variable slots. No adapted field/Value access precedes
            // these reference fences. Changed helpers/delegates/state use the
            // entire original PowerShell route, before any new native call.
            if (!(currentValue is object[] current) || !(ownersValue is object[] owners) || current.Length != 42 || owners.Length != 42) return null;
            for (int i = 0; i < current.Length; i++) if (!Object.ReferenceEquals(current[i], owners[i])) return null;
            if (!(current[1] is ScriptBlock)) return null;
            if (!(current[16] is List<IntPtr> handleOwner)) return null;
            lock (ownerLock)
            {
                if (!Reader.owners.TryGetValue(handleOwner, out Ownership owner) || owner.Closed ||
                    owner.Memory != memory || owner.Kernel != kernel) return null;
            }
            if (scratchBusy || memory == IntPtr.Zero || memory != memoryOwner || kernel != kernelOwner || !(entries is int e) || e != 0 ||
                !(bytes is ulong b) || b != 0 || !(metadata is int m) || m != 0 ||
                !(units is int u) || !(entryLimit is int el) || el != 8192 ||
                !(unitLimit is int ul) || ul != 1048576 || !(chainLimit is int cl) || cl != 512 || !Catalog(catalog)) return null;
            if (current[15]?.GetType() != typeof(Hashtable) ||
                current[16]?.GetType() != typeof(List<IntPtr>) ||
                current[17]?.GetType() != typeof(Dictionary<string, object>) ||
                current[18]?.GetType() != typeof(List<object>) ||
                current[19]?.GetType() != typeof(Dictionary<string, string>) ||
                current[20]?.GetType() != typeof(Stopwatch) ||
                current[21]?.GetType() != typeof(byte[]) || current[22]?.GetType() != typeof(byte[])) return null;
            var limits = (Hashtable)current[15]; var handles = (List<IntPtr>)current[16];
            var directories = (Dictionary<string, object>)current[17]; var records = (List<object>)current[18];
            var memo = (Dictionary<string, string>)current[19]; var watch = (Stopwatch)current[20];
            var scratch = (byte[])current[21]; var buffer = (byte[])current[22];
            if (handles.Count != 0 || directories.Count != 0 || records.Count != 0 || scratch.Length != 52 || buffer.Length != 1048576 ||
                memo.Count != 1 || String.IsNullOrEmpty(systemRoot) || !memo.TryGetValue(systemRoot, out string cached) ||
                !String.Equals(cached, systemRoot, StringComparison.Ordinal) || u != systemRoot.Length * 2) return null;
            string[] keys = { "Entries", "Handles", "FileBytes", "TotalBytes", "Depth", "JsonBytes", "MetadataBytes", "Seconds" };
            long[] values = { 200000, 200256, 2147483648, 68719476736, 64, 33554432, 31457280, 180 };
            if (limits.Count != keys.Length) return null;
            for (int i = 0; i < keys.Length; i++)
            {
                if (!NoMember(limits, keys[i])) return null;
                object value = limits[keys[i]];
                if (i == 2 || i == 3) { if (!(value is long l) || l != values[i]) return null; }
                else if (!(value is int integer) || integer != values[i]) return null;
            }
            if (!Property(watch, "Elapsed") || !Property(TimeSpan.Zero, "TotalSeconds") ||
                !Property(scratch, "Length") || !Property(buffer, "Length") || !Property(handles, "Count")) return null;
            var table = Table(0);
            foreach (string key in new[] { "attributes", "directory", "bytes", "write", "links", "identity", "path", "handle", "initial", "comparison", "depth", "names", "root", "files", "sha256", "fact", "text" })
                if (!NoMember(table, key)) return null;
            for (int i = 11; i <= 14; i++) if (!(current[i] is Delegate) || !Method(current[i], "Invoke")) return null;
            if (kernel == IntPtr.Zero) return null;
            string[] exports = { "CreateFileW", "GetFileType", "GetFileInformationByHandle", "CloseHandle" };
            for (int i = 0; i < exports.Length; i++)
                if (Marshal.GetFunctionPointerForDelegate((Delegate)current[11 + i]) != NativeLibrary.GetExport(kernel, exports[i])) return null;
            // These ordinary state instances cannot execute callbacks. All
            // factories used later are covered by the type catalog above.
            lock (ownerLock)
            {
                if (!Reader.owners.TryGetValue(handles, out Ownership owner) || owner.Closed ||
                    owner.Memory != memory || owner.Kernel != kernel) return null;
                if (ownedReaders.TryGetValue(handles, out Reader existing)) return null;
                var reader = new Reader(handles, directories, records, memo, u, watch, scratch, buffer, memory,
                (Delegate)current[11], (Delegate)current[12], (Delegate)current[13], limits, (ScriptBlock)current[1]);
                ownedReaders.Add(handles, reader);
                return reader;
            }
        }

        public void Retire()
        {
            lock (ownerLock)
            {
                if (active || busy) Fail("identity");
                if (owners.TryGetValue(Handles, out Ownership owner))
                { owner.Closed = true; owner.Memory = IntPtr.Zero; owner.Kernel = IntPtr.Zero; }
                retired = true; Memory = IntPtr.Zero; lastPath = null;
                if (lastChain != null) Array.Clear(lastChain, 0, lastChain.Length);
                lastChain = null; directoryRecords.Clear(); fileRecords.Clear();
                // Keep a weak closed-owner latch: the same initializing list
                // cannot admit a replacement reader while Close is freeing
                // its handles/scratch. A new Initialize creates a new list.
            }
        }

        public static object[] Statistics(Reader reader)
        {
            object[] chain = null;
            if (reader.lastChain != null)
            {
                chain = new object[reader.lastChain.Length];
                for (int i = 0; i < chain.Length; i++) chain[i] = reader.lastChain[i].Public;
            }
            return new object[] { reader.Entries, reader.Bytes, reader.Metadata, reader.MemoUnits,
                reader.CompletedFiles, reader.InitialReads, reader.Stage, reader.Work, reader.Cause,
                reader.CompletedRoots, reader.lastPath, chain };
        }
        public static void RetireOwned(Reader reader) { if (reader != null) reader.Retire(); }
        public static void RetireOwner(object ownerValue)
        {
            if (!(ownerValue is object[] owners) || owners.Length != 42 || !(owners[16] is List<IntPtr> handles)) return;
            lock (ownerLock)
            {
                if (ownedReaders.TryGetValue(handles, out Reader reader)) reader.Retire();
                else if (Reader.owners.TryGetValue(handles, out Ownership owner))
                { owner.Closed = true; owner.Memory = IntPtr.Zero; owner.Kernel = IntPtr.Zero; }
            }
        }
        public static OrderedDictionary RunOwnedToolchain(Reader reader, string systemRoot) { return reader.RunToolchain(systemRoot); }
        public static Hashtable RunOwnedRoot(Reader reader, string root) { return reader.RunRoot(root); }
        public static string ValidateOwnedPath(Reader reader, string value)
        {
            lock (ownerLock)
            {
                if (reader.retired || reader.active || reader.entered) Fail("identity");
                reader.active = true;
            }
            try { return reader.Canonical(value); }
            finally { lock (ownerLock) { reader.active = false; } }
        }

        private static Hashtable Table(int capacity) { return new Hashtable(capacity, StringComparer.OrdinalIgnoreCase); }
        public static OrderedDictionary NewOrderedMap() { return new OrderedDictionary(StringComparer.OrdinalIgnoreCase); }
        private static bool Equal(string a, string b, bool ignoreCase)
        { return LanguagePrimitives.Equals(a, b, ignoreCase, CultureInfo.InvariantCulture); }
        private static bool Match(string value, string pattern)
        { return Regex.IsMatch(value, pattern, RegexOptions.IgnoreCase); }
        private static void Fail(string reason) { throw new InvalidOperationException(reason); }
        private void BudgetFailure(string cause)
        { if (Cause == "unknown") Cause = cause; Fail("limit"); }

        private void Budget()
        {
            // Same five predicates and short-circuit order as Assert-HostBudget.
            if (Watch.Elapsed.TotalSeconds > 180) BudgetFailure("deadline");
            else if (Entries > 200000) BudgetFailure("entries");
            else if (Bytes > 68719476736UL) BudgetFailure("bytes");
            else if (Handles.Count > 200256) BudgetFailure("handles");
            else if (Metadata > 31457280) BudgetFailure("metadata");
        }

        private string Canonical(string value)
        {
            string full;
            if (!String.IsNullOrEmpty(value) && Memo.TryGetValue(value, out full)) return full;
            if (value != null && value.Length > 1024) Fail("path_length");
            bool unicode = false;
            if (value != null) foreach (char unit in value) if (unit > 127) { unicode = true; break; }
            if (unicode)
            {
                // Preserve the original PowerShell Regex cache/culture and
                // Unicode behavior in a fresh local scope. Exact owner slot1
                // fences this original pure helper; no filesystem/native call,
                // supplied script, custom method or getter is introduced.
                var result = unicodeValidator.InvokeWithContext((Dictionary<string, ScriptBlock>)null,
                    (List<PSVariable>)null, new object[] { value });
                if (result.Count != 1 || !(result[0].BaseObject is string)) Fail("path_input");
                full = (string)result[0].BaseObject;
            }
            else full = ValidateAsciiPath(value);
            int units = value.Length + full.Length;
            if (Memo.Count < 8192 && units <= 1048576 - MemoUnits)
            { Memo.Add(value, full); MemoUnits += units; }
            return full;
        }

        private static string ValidateAsciiPath(string value)
        {
            // ASCII-only old predicates/order and invariant PowerShell string
            // equality. Non-ASCII uses the exact original pure helper above.
            if (String.IsNullOrEmpty(value)) Fail("path_empty");
            if (value.Length > 1024) Fail("path_length");
            if (!Match(value, @"^[A-Za-z]:\\")) Fail("path_drive");
            if (value.Substring(2).Contains(':')) Fail("path_colon");
            if (value.Contains('/')) Fail("path_slash");
            if (Match(value, "[\\x00-\\x1f*?\"<>|]")) Fail("path_chars");
            string full = Path.GetFullPath(value);
            if (!Equal(full.TrimEnd('\\'), value.TrimEnd('\\'), false)) Fail("path_normalized");
            foreach (string part in value.Substring(3).Split('\\'))
            {
                if (Equal(part, "", true) && value.Length == 3) continue;
                if (Equal(part, "", true)) Fail("path_part_empty");
                if (Equal(part, ".", true) || Equal(part, "..", true)) Fail("path_part_dot");
                if (part.EndsWith(".")) Fail("path_part_trailingdot");
                if (part.EndsWith(" ")) Fail("path_part_trailingspace");
                if (Match(part, @"^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)")) Fail("path_part_device");
                if (Equal(part, ".git", true)) Fail("path_part_git");
            }
            return full;
        }

        private Hashtable ReadInfo(IntPtr handle, byte[] expected, bool bound, bool content)
        {
            if (busy || Memory == IntPtr.Zero) Fail("identity");
            busy = true;
            try
            {
                if (type(handle) != 1) Fail("type");
                if (info(handle, Memory) == 0) Fail("identity");
                Marshal.Copy(Memory, Scratch, 0, 52);
                uint attributes = BitConverter.ToUInt32(Scratch, 0);
                if ((attributes & 0x400) != 0) Fail("reparse");
                if (bound)
                {
                    if (expected == null || expected.Length != 52) Fail("identity");
                    if (attributes != BitConverter.ToUInt32(expected, 0) ||
                        BitConverter.ToUInt32(Scratch, 28) != BitConverter.ToUInt32(expected, 28) ||
                        BitConverter.ToUInt64(Scratch, 44) != BitConverter.ToUInt64(expected, 44) ||
                        (content && (BitConverter.ToUInt64(Scratch, 32) != BitConverter.ToUInt64(expected, 32) ||
                         BitConverter.ToUInt64(Scratch, 20) != BitConverter.ToUInt64(expected, 20) ||
                         BitConverter.ToUInt32(Scratch, 40) != BitConverter.ToUInt32(expected, 40)))) Fail("changed");
                    return null;
                }
                var result = Table(6);
                result.Add("attributes", attributes);
                result.Add("directory", (attributes & 0x10) != 0);
                result.Add("bytes", (ulong)BitConverter.ToUInt32(Scratch, 32) * 4294967296UL + BitConverter.ToUInt32(Scratch, 36));
                result.Add("write", Convert.ToHexString(Scratch, 20, 8));
                result.Add("links", BitConverter.ToUInt32(Scratch, 40));
                result.Add("identity", Convert.ToHexString(Scratch, 28, 4) + Convert.ToHexString(Scratch, 44, 8));
                if (InitialReads < 200000) InitialReads++;
                return result;
            }
            finally { busy = false; }
        }

        private Record Open(string literal, bool directory)
        {
            Budget();
            string full = Canonical(literal);
            IntPtr wide = Marshal.StringToHGlobalUni(@"\\?\" + full), handle;
            try
            {
                // Full forward payload reads use the Windows cache hint while
                // retaining no-follow and the original directory flags.
                handle = open(wide, directory ? 128U : 2147483648U, 1U,
                    IntPtr.Zero, 3U, directory ? 0x02200000U : 0x08200000U, IntPtr.Zero);
            }
            finally { Marshal.FreeHGlobal(wide); }
            if (handle == new IntPtr(-1)) Fail("open");
            Handles.Add(handle); Budget();
            Hashtable initial = ReadInfo(handle, null, false, false);
            if ((bool)initial["directory"] != directory) Fail("type");
            byte[] comparison = (byte[])Scratch.Clone();
            var record = new Record { Path = full, Handle = handle, Initial = initial, Comparison = comparison };
            var export = Table(4);
            export.Add("path", full); export.Add("handle", handle);
            export.Add("initial", initial); export.Add("comparison", comparison);
            record.Public = export;
            return record;
        }

        private Record Chain(string literal)
        {
            string full = Canonical(literal);
            if (lastChain != null && lastChain.Length <= 512 && String.Equals(full, lastPath, StringComparison.Ordinal))
            {
                foreach (Record record in lastChain) ReadInfo(record.Handle, record.Comparison, true, false);
                return lastChain[lastChain.Length - 1];
            }
            lastPath = null; lastChain = null;
            var chain = new List<Record>();
            bool remember = true;
            string current = full.Substring(0, 3);
            var parts = new List<string> { "" };
            foreach (string part in full.Substring(3).Split('\\')) if (!Equal(part, "", true)) parts.Add(part);
            foreach (string part in parts)
            {
                if (part != "") current = Path.Combine(current, part);
                Record record;
                if (!directoryRecords.TryGetValue(current, out record))
                {
                    record = Open(current, true);
                    directoryRecords.Add(current, record);
                    Directories.Add(current, record.Public);
                }
                else ReadInfo(record.Handle, record.Comparison, true, false);
                if (remember)
                {
                    if (chain.Count < 512) chain.Add(record);
                    else { remember = false; chain.Clear(); }
                }
            }
            if (remember) { lastPath = full; lastChain = chain.ToArray(); }
            return directoryRecords[full];
        }

        private Hashtable FileFact(string literal, int textLimit, ulong remaining)
        {
            Work = "file_open";
            string full = Canonical(literal);
            Chain(Path.GetDirectoryName(full));
            Record record = Open(full, false);
            fileRecords.Add(record); FileRecords.Add(record.Public);
            ulong expected = (ulong)record.Initial["bytes"];
            if (expected > 2147483648UL) { if (Cause == "unknown") Cause = "bytes"; Fail("file_size"); }
            if (textLimit > 0 && expected > (ulong)textLimit) { if (Cause == "unknown") Cause = "bytes"; Fail("text_size"); }
            if (expected > remaining) { if (Cause == "unknown") Cause = "bytes"; Fail("limit"); }
            var safe = new SafeFileHandle(record.Handle, false);
            FileStream stream = null;
            MemoryStream text = null;
            try
            {
                // Keep stream/provider/text construction in original order,
                // but release borrowed wrappers even if provider creation fails.
                stream = new FileStream(safe, FileAccess.Read, 1048576, false);
                if (payloadDigest == null) payloadDigest = SHA256.Create();
                SHA256 digest = payloadDigest;
                text = textLimit > 0 ? new MemoryStream() : null;
                Work = "file_hash";
                ulong size = 0;
                int count;
                while ((count = stream.Read(Buffer, 0, Buffer.Length)) > 0)
                {
                    Budget(); size += (ulong)count;
                    if (size > expected) Fail("changed");
                    digest.TransformBlock(Buffer, 0, count, null, 0);
                    if (text != null) text.Write(Buffer, 0, count);
                }
                digest.TransformFinalBlock(new byte[0], 0, 0);
                if (size != expected) Fail("changed");
                if (CompletedFiles < 200000) CompletedFiles++;
                Work = "file_identity";
                ReadInfo(record.Handle, record.Comparison, true, true);
                var fact = Table(2);
                fact.Add("bytes", size); fact.Add("sha256", Convert.ToHexString(digest.Hash).ToLowerInvariant());
                if (text != null) fact["text"] = new UTF8Encoding(false, true).GetString(text.ToArray());
                return fact;
            }
            finally
            {
                try { if (stream != null) stream.Dispose(); }
                finally
                {
                    try { safe.Dispose(); }
                    finally { if (text != null) text.Dispose(); }
                }
            }
        }

        private string[] Names(string directory)
        {
            var names = new List<string>();
            var folded = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (string path in Directory.EnumerateFileSystemEntries(directory))
            {
                Budget();
                string name = Path.GetFileName(path);
                Canonical(Path.Combine(directory, name));
                if (!folded.Add(name)) Fail("alias");
                names.Add(name);
                if (names.Count > 200000) { if (Cause == "unknown") Cause = "entries"; Fail("entries"); }
            }
            string[] result = names.ToArray(); Array.Sort(result, StringComparer.Ordinal);
            return result;
        }

        private Hashtable Root(string literal)
        {
            string root = Canonical(literal);
            Work = "directory_open"; Chain(root);
            var pending = new Stack<Pending>(); pending.Push(new Pending { Path = root, Depth = 0 });
            var snapshots = new List<Snapshot>(); var files = new List<object>();
            while (pending.Count > 0)
            {
                Budget(); Pending next = pending.Pop();
                if (next.Depth > 64) Fail("depth");
                Work = "directory_open"; Record directory = Chain(next.Path);
                Work = "enumeration"; string[] names = Names(next.Path);
                snapshots.Add(new Snapshot { Directory = directory, Names = names });
                foreach (string name in names)
                {
                    Entries++; Budget(); string path = Path.Combine(next.Path, name);
                    Work = "entry_type"; FileAttributes attributes = File.GetAttributes(path);
                    if ((attributes & FileAttributes.ReparsePoint) != 0) Fail("reparse");
                    if ((attributes & FileAttributes.Directory) != 0)
                    {
                        Work = "directory_open"; Chain(path);
                        pending.Push(new Pending { Path = path, Depth = next.Depth + 1 });
                    }
                    else
                    {
                        Hashtable fact = FileFact(path, 0, 68719476736UL - Bytes);
                        Work = "receipt_row"; Bytes += (ulong)fact["bytes"]; Budget();
                        string relative = Path.GetRelativePath(root, path).Replace('\\', '/');
                        Metadata += 160 + 6 * relative.Length; Budget();
                        var row = Table(3); row.Add("path", relative); row.Add("bytes", fact["bytes"]); row.Add("sha256", fact["sha256"]);
                        files.Add(row);
                    }
                }
            }
            foreach (Snapshot snapshot in snapshots)
            {
                Work = "snapshot_identity"; Budget(); ReadInfo(snapshot.Directory.Handle, snapshot.Directory.Comparison, true, true);
                Work = "snapshot_names"; string[] again = Names(snapshot.Directory.Path);
                if (again.Length != snapshot.Names.Length) Fail("changed");
                for (int i = 0; i < again.Length; i++) if (!Equal(again[i], snapshot.Names[i], false)) Fail("changed");
            }
            var result = Table(2); result.Add("root", root); result.Add("files", files.ToArray());
            if (CompletedRoots < 2) CompletedRoots++;
            return result;
        }

        private void Enter()
        {
            // Never import or reinterpret previously exposed/possibly mutated
            // records. A reader performs either this complete call or the old
            // path. No partial replay or post-native admission failure exists.
            lock (ownerLock)
            {
                if (!owners.TryGetValue(Handles, out Ownership owner) || owner.Closed ||
                    retired || entered || active || Handles.Count != 0 || Directories.Count != 0 || FileRecords.Count != 0 ||
                    Entries != 0 || Bytes != 0 || Metadata != 0 || Memory == IntPtr.Zero || Scratch.Length != 52 || Buffer.Length != 1048576)
                    Fail("identity");
                entered = true; active = true;
            }
        }

        private void DisposePayloadDigest()
        {
            SHA256 digest = payloadDigest;
            payloadDigest = null;
            if (digest != null) digest.Dispose();
        }

        public Hashtable RunRoot(string root)
        {
            Enter();
            try { return Root(root); }
            finally
            {
                try { DisposePayloadDigest(); }
                finally { lock (ownerLock) { active = false; } }
            }
        }

        public OrderedDictionary RunToolchain(string systemRoot)
        {
            Enter();
            try { return Toolchain(systemRoot); }
            finally
            {
                try { DisposePayloadDigest(); }
                finally { lock (ownerLock) { active = false; } }
            }
        }

        private OrderedDictionary Toolchain(string systemRoot)
        {
            Stage = "role_binding";
            string drive = systemRoot.Substring(0, 2);
            string vsBase = drive + @"\Program Files\Microsoft Visual Studio\2022";
            var candidates = new List<string>();
            foreach (string edition in new[] { "Enterprise", "Professional", "Community", "Preview", "BuildTools" })
            { string path = Path.Combine(vsBase, edition); if (Directory.Exists(path)) candidates.Add(path); }
            if (candidates.Count != 1) Fail("visual_studio_selection");
            string vs = Canonical(candidates[0]), sdk = drive + @"\Program Files (x86)\Windows Kits\10";
            string versionFile = Path.Combine(vs, "VC", "Auxiliary", "Build", "Microsoft.VCToolsVersion.default.txt");
            Hashtable versionFact = FileFact(versionFile, 256, UInt64.MaxValue);
            string version = ((string)versionFact["text"]).Trim();
            if (!Match(version, @"^14\.[0-9]{1,3}\.[0-9]{1,6}$")) Fail("msvc_version");
            string msvc = Path.Combine(vs, "VC", "Tools", "MSVC", version);
            string vcvars = Path.Combine(vs, "VC", "vcvarsall.bat");
            if (!File.Exists(vcvars)) vcvars = Path.Combine(vs, "VC", "Auxiliary", "Build", "vcvarsall.bat");
            var roles = new OrderedDictionary(StringComparer.OrdinalIgnoreCase) {
                { "vcvarsall", vcvars }, { "msvc_default_version", versionFile },
                { "cl", Path.Combine(msvc, "bin", "Hostx64", "x64", "cl.exe") },
                { "link", Path.Combine(msvc, "bin", "Hostx64", "x64", "link.exe") },
                { "msvc_include", Path.Combine(msvc, "include", "vcruntime.h") },
                { "msvc_lib", Path.Combine(msvc, "lib", "x64", "vcruntime.lib") },
                { "sdk_rc", Path.Combine(sdk, "bin", "10.0.26100.0", "x64", "rc.exe") },
                { "sdk_mt", Path.Combine(sdk, "bin", "10.0.26100.0", "x64", "mt.exe") },
                { "sdk_um", Path.Combine(sdk, "Include", "10.0.26100.0", "um", "Windows.h") },
                { "sdk_shared", Path.Combine(sdk, "Include", "10.0.26100.0", "shared", "sdkddkver.h") },
                { "sdk_ucrt", Path.Combine(sdk, "Include", "10.0.26100.0", "ucrt", "stdlib.h") },
                { "sdk_kernel_lib", Path.Combine(sdk, "Lib", "10.0.26100.0", "um", "x64", "kernel32.lib") },
                { "sdk_ucrt_lib", Path.Combine(sdk, "Lib", "10.0.26100.0", "ucrt", "x64", "ucrt.lib") }
            };
            var roleFacts = NewOrderedMap();
            foreach (DictionaryEntry role in roles)
            {
                Hashtable fact = FileFact((string)role.Value, 0, UInt64.MaxValue);
                if ((ulong)fact["bytes"] == 0) Fail("role");
                var row = Table(2); row.Add("path", role.Value); row.Add("fact", fact); roleFacts.Add(role.Key, row);
            }
            Stage = "vs_inventory"; Hashtable vsInventory = Root(vs);
            Stage = "sdk_inventory"; Hashtable sdkInventory = Root(sdk);
            Stage = "final_identity"; Work = "final_directory";
            foreach (Record record in directoryRecords.Values) ReadInfo(record.Handle, record.Comparison, true, false);
            Work = "final_file";
            foreach (Record record in fileRecords) { Budget(); ReadInfo(record.Handle, record.Comparison, true, true); }
            Budget();
            return new OrderedDictionary(StringComparer.OrdinalIgnoreCase) {
                { "schema", "morpheus-native-windows-host-toolchain-inventory-v2" },
                { "visual_studio_root", vs }, { "visual_studio_year", "2022" },
                { "msvc_toolset_version", version }, { "windows_sdk_version", "10.0.26100.0" },
                { "roots", new object[] { vsInventory, sdkInventory } }, { "roles", roleFacts },
                { "upstream_build_revision", "be1a8f6dcd7df7e46320192c5e2f364e50d79bbf" },
                { "upstream_vs_toolchain_sha256", "d13a7b17b81c2084de5a76e907c458ca997a4ddcdef96d3b74c8fcb3aca5f463" },
                { "upstream_setup_toolchain_sha256", "f6ae24c99ae0bfc72b093da415615119652f25415155dffb4db22f49b78ca8cf" },
                { "entries", Entries }, { "total_bytes", Bytes }, { "limits", Limits },
                { "environment_realized", false }, { "vcvars_or_compilers_executed", true },
                { "preflight_implementation", "owned-managed-transaction-v1" },
                { "preflight_compiler", "powershell-bundled-roslyn-in-process-v1" },
                { "external_toolchain_executables_executed", false },
                { "host_approval_granted", false }, { "build_ready", false }, { "distribution_ready", false }
            };
        }
    }
}

'@
    $null = Microsoft.PowerShell.Utility\Add-Type -TypeDefinition $source -Language CSharp -CompilerOptions '/optimize+' -PassThru
    $script:HostTraversalCompilerReady = $true
    return [Morpheus.OwnedTraversal20261028.Reader]
}

function New-HostOwnedTraversal {
    $current = [object[]]@(${function:Assert-HostBudget}, ${function:Get-CanonicalHostPath}, ${function:Get-HostCanonicalPath},
        ${function:Open-HostReadHandle}, ${function:Get-HostHandleInfo}, ${function:Assert-HostIdentity},
        ${function:Open-HostDirectoryChain}, ${function:Get-HostDirectoryNames}, ${function:Get-HostRootInventory},
        ${function:Get-HostToolchainInventory}, ${function:Close-HostReader},
        $script:HostOpen, $script:HostType, $script:HostInfo, $script:HostClose,
        $script:HostLimits, $script:HostHandles, $script:HostDirectories, $script:HostFileRecords,
        $script:HostCanonicalPaths, $script:HostWatch, $script:HostInfoScratch, $script:HostBuffer,
        $script:HostInitialInvoker, $script:HostComparisonInvoker, $script:HostChainInvoker,
        $script:HostHashInvoker, $script:HostHashAdmissionInvoker, $script:HostCanonicalInvoker,
        $script:HostInitialInvokerOwner, $script:HostHashAdmissionOwner, $script:HostHashInvokerOwner,
        $script:HostComparisonType, $script:HostComparisonInfo, $script:HostChainInvokerOwner,
        $script:HostChainComparison, $script:HostHashBudgetOwner, $script:HostHashLimitsOwner,
        $script:HostHashHandlesOwner, $script:HostHashOpenOwner, $script:HostHashInfoOwner,
        ${function:Get-HostFileFact})
    $catalog = [Management.Automation.Runspaces.TypeData[]]@(Microsoft.PowerShell.Utility\Get-TypeData)
    $type = Get-HostOwnedTraversalType
    $reader = $type::TryCreate($current, $script:HostTraversalOwners, $catalog,
        $script:HostInfoMemory, $script:HostTraversalMemoryOwner,
        $script:HostKernel, $script:HostTraversalKernelOwner, $script:HostInfoBusy,
        $script:HostCount, $script:HostBytes, $script:HostMetadataBytes,
        $script:HostCanonicalUnits, $script:HostCanonicalEntryLimit, $script:HostCanonicalUnitLimit,
        $script:HostDirectoryChainLimit, $env:SystemRoot)
    if ($null -ne $reader) { $script:HostTraversalReader = $reader }
    return $reader
}

# V2 distinguishes traversal implementation from compiler/tool execution.
# Even an unsupported-state fallback has invoked the bundled compiler while
# constructing its eligibility helper; it must never emit the old false flag.
function Set-HostInventoryImplementation([Collections.IDictionary] $Inventory, [string] $Implementation) {
    if (-not $script:HostTraversalCompilerReady -or
        (-not [string]::Equals($Implementation, 'owned-managed-transaction-v1', [StringComparison]::Ordinal) -and
         -not [string]::Equals($Implementation, 'powershell-original-fallback-v1', [StringComparison]::Ordinal))) {
        throw 'receipt_provenance'
    }
    $Inventory['schema'] = 'morpheus-native-windows-host-toolchain-inventory-v2'
    $Inventory['vcvars_or_compilers_executed'] = $true
    $Inventory['preflight_implementation'] = $Implementation
    $Inventory['preflight_compiler'] = 'powershell-bundled-roslyn-in-process-v1'
    $Inventory['external_toolchain_executables_executed'] = $false
    return $Inventory
}

function Invoke-HostOwnedTraversal($Reader, [string] $Root, [bool] $RootOnly = $false) {
    if ($script:HostInfoBusy) { throw 'identity' }
    $script:HostTraversalReader = $Reader
    $script:HostInfoBusy = $true
    try {
        if ($RootOnly) { $result = [Morpheus.OwnedTraversal20261028.Reader]::RunOwnedRoot($Reader, $Root) }
        else { $result = [Morpheus.OwnedTraversal20261028.Reader]::RunOwnedToolchain($Reader, $env:SystemRoot) }
    } finally {
        try {
            $state = [Morpheus.OwnedTraversal20261028.Reader]::Statistics($Reader)
            $script:HostCount = $state[0]; $script:HostBytes = $state[1]; $script:HostMetadataBytes = $state[2]
            $script:HostCanonicalUnits = $state[3]; $script:HostManagedHashes = $state[4]
            $script:HostManagedInitialInfos = $state[5]; $script:HostPreflightStage = $state[6]
            $script:HostPreflightWork = $state[7]; $script:HostPreflightBudgetCause = $state[8]
            $script:HostManagedTraversalRoots = $state[9]
            $script:HostLastDirectoryPath = $state[10]; $script:HostLastDirectoryChain = $state[11]
        } finally { $script:HostInfoBusy = $false }
    }
    if (-not $RootOnly) {
        $result = Set-HostInventoryImplementation $result 'owned-managed-transaction-v1'
    }
    return $result
}

function Initialize-HostReader {
    # One sequential reader owns its metadata scratch until Close-HostReader.
    # Refuse an overlapping initialization rather than lose live owned storage.
    $previous = Get-Variable -Name HostInfoMemory -Scope Script -ErrorAction SilentlyContinue
    $previousHandles = Get-Variable -Name HostHandles -Scope Script -ErrorAction SilentlyContinue
    if (($null -ne $previous -and $previous.Value -ne [IntPtr]::Zero) -or
        ($null -ne $previousHandles -and $previousHandles.Value.Count -ne 0)) { throw 'identity' }
    $script:HostPreflightBudgetCause = 'unknown'
    $script:HostPreflightWork = 'idle'
    $script:HostManagedHashes = 0
    $script:HostManagedAdmissions = 0
    $script:HostManagedInitialInfos = 0
    $script:HostManagedTraversalRoots = 0
    $script:HostTraversalReader = $null
    $script:HostTraversalCompilerReady = $false
    $script:HostTraversalOwners = $null
    $script:HostTraversalMemoryOwner = [IntPtr]::Zero
    $script:HostTraversalKernelOwner = [IntPtr]::Zero
    $script:HostInfoMemory = [IntPtr]::Zero
    $script:HostInfoScratch = $null
    $script:HostInfoBusy = $false
    $script:HostComparisonInvoker = $null
    $script:HostComparisonType = $null
    $script:HostComparisonInfo = $null
    $script:HostInitialInvoker = $null
    $script:HostInitialInvokerOwner = $null
    $script:HostHashAdmissionInvoker = $null
    $script:HostHashAdmissionOwner = $null
    $script:HostChainInvoker = $null
    $script:HostChainInvokerOwner = $null
    $script:HostChainComparison = $null
    $script:HostHashInvoker = $null
    $script:HostHashInvokerOwner = $null
    $script:HostHashBudgetOwner = $null
    $script:HostHashLimitsOwner = $null
    $script:HostHashHandlesOwner = $null
    $script:HostHashOpenOwner = $null
    $script:HostHashInfoOwner = $null
    $script:HostHandles = [Collections.Generic.List[IntPtr]]::new()
    $script:HostCanonicalPaths = $null
    $script:HostCanonicalInvoker = $null
    $script:HostCanonicalUnits = 0
    # Only immutable lexical results are memoized. Count and UTF-16 code-unit
    # limits bound this reader-owned cache separately from receipt limits.
    $script:HostCanonicalEntryLimit = 8192
    $script:HostCanonicalUnitLimit = 1048576
    $script:HostCanonicalPaths = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
    # These are fixed source limits, never manifest-selected overrides.
    $script:HostLimits = @{ Entries = 200000; Handles = 200256; FileBytes = 2147483648;
        TotalBytes = 68719476736; Depth = 64; JsonBytes = 33554432; MetadataBytes = 31457280; Seconds = 180 }
    $script:HostWatch = [Diagnostics.Stopwatch]::StartNew()
    $script:HostCount = 0; $script:HostBytes = [uint64]0; $script:HostMetadataBytes = 0
    $script:HostBuffer = [byte[]]::new(1048576)
    $script:HostFileRecords = [Collections.Generic.List[object]]::new()
    $script:HostDirectories = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
    # One exact directory's immutable lexical prefix-to-record association.
    # Every cached record still receives a fresh native identity check per use.
    $script:HostLastDirectoryPath = $null
    $script:HostLastDirectoryChain = $null
    $script:HostDirectoryChainLimit = 512
    $script:HostHashAdmissionInvoker = New-HostHashAdmissionInvoker
    $script:HostHashAdmissionOwner = $script:HostHashAdmissionInvoker
    $script:HostHashInvoker = New-HostHashInvoker
    $script:HostHashInvokerOwner = $script:HostHashInvoker
    $script:HostHashBudgetOwner = ${function:Assert-HostBudget}
    $script:HostHashLimitsOwner = $script:HostLimits
    $script:HostHashHandlesOwner = $script:HostHandles
    $script:HostHashOpenOwner = ${function:Open-HostReadHandle}
    $script:HostHashInfoOwner = ${function:Get-HostHandleInfo}
    # Pure lexical work is compiled before acquiring any unmanaged ownership.
    $script:HostCanonicalInvoker = New-HostCanonicalInvoker
    $system = Get-HostCanonicalPath $env:SystemRoot
    if ($system -cnotmatch '^[A-Za-z]:\\Windows$') { throw 'host' }
    $script:HostKernel = [Runtime.InteropServices.NativeLibrary]::Load([IO.Path]::Combine($system, 'System32', 'kernel32.dll'))
    $script:HostOpen = New-HostReadDelegate 'CreateFileW' ([IntPtr]) ([Type[]]@([IntPtr], [uint32], [uint32], [IntPtr], [uint32], [uint32], [IntPtr]))
    $script:HostInfo = New-HostReadDelegate 'GetFileInformationByHandle' ([int]) ([Type[]]@([IntPtr], [IntPtr]))
    $script:HostType = New-HostReadDelegate 'GetFileType' ([uint32]) ([Type[]]@([IntPtr]))
    $script:HostClose = New-HostReadDelegate 'CloseHandle' ([int]) ([Type[]]@([IntPtr]))
    # Emit before acquiring unmanaged scratch; a construction error cannot
    # strand an owned allocation. The exact delegate objects fence this path.
    $script:HostComparisonInvoker = New-HostComparisonInvoker $script:HostType $script:HostInfo
    $script:HostComparisonType = $script:HostType
    $script:HostComparisonInfo = $script:HostInfo
    $script:HostInitialInvoker = New-HostInitialInvoker $script:HostType $script:HostInfo
    $script:HostInitialInvokerOwner = $script:HostInitialInvoker
    $script:HostChainInvoker = New-HostChainInvoker $script:HostComparisonInvoker
    $script:HostChainInvokerOwner = $script:HostChainInvoker
    $script:HostChainComparison = $script:HostComparisonInvoker
    # Compile/register before acquiring unmanaged scratch. Every initializing
    # handles-list owner gets a private closed latch, even if no Reader admits.
    $ownedType = Get-HostOwnedTraversalType
    $ownedType::PrepareOwner($script:HostHandles)
    # Capture provenance before allocation. Guard the additional owner-pointer
    # publication so a failed assignment cannot strand the 52-byte allocation.
    $script:HostInfoScratch = [byte[]]::new(52)
    # Capture exact object/function provenance before acquiring unmanaged storage.
    $script:HostTraversalOwners = [object[]]@(${function:Assert-HostBudget}, ${function:Get-CanonicalHostPath}, ${function:Get-HostCanonicalPath},
        ${function:Open-HostReadHandle}, ${function:Get-HostHandleInfo}, ${function:Assert-HostIdentity},
        ${function:Open-HostDirectoryChain}, ${function:Get-HostDirectoryNames}, ${function:Get-HostRootInventory},
        ${function:Get-HostToolchainInventory}, ${function:Close-HostReader},
        $script:HostOpen, $script:HostType, $script:HostInfo, $script:HostClose,
        $script:HostLimits, $script:HostHandles, $script:HostDirectories, $script:HostFileRecords,
        $script:HostCanonicalPaths, $script:HostWatch, $script:HostInfoScratch, $script:HostBuffer,
        $script:HostInitialInvoker, $script:HostComparisonInvoker, $script:HostChainInvoker,
        $script:HostHashInvoker, $script:HostHashAdmissionInvoker, $script:HostCanonicalInvoker,
        $script:HostInitialInvokerOwner, $script:HostHashAdmissionOwner, $script:HostHashInvokerOwner,
        $script:HostComparisonType, $script:HostComparisonInfo, $script:HostChainInvokerOwner,
        $script:HostChainComparison, $script:HostHashBudgetOwner, $script:HostHashLimitsOwner,
        $script:HostHashHandlesOwner, $script:HostHashOpenOwner, $script:HostHashInfoOwner,
        ${function:Get-HostFileFact})
    $script:HostTraversalKernelOwner = $script:HostKernel
    $ownedMemory = [Runtime.InteropServices.Marshal]::AllocHGlobal(52)
    try {
        $script:HostInfoMemory = $ownedMemory
        $script:HostTraversalMemoryOwner = $ownedMemory
        [Morpheus.OwnedTraversal20261028.Reader]::PublishOwner($script:HostHandles,
            $ownedMemory, $script:HostKernel)
    } catch {
        [Morpheus.OwnedTraversal20261028.Reader]::RetireOwner($script:HostTraversalOwners)
        [Runtime.InteropServices.Marshal]::FreeHGlobal($ownedMemory)
        $script:HostInfoMemory = [IntPtr]::Zero
        $script:HostTraversalMemoryOwner = [IntPtr]::Zero
        throw
    }
}

function Assert-HostBudget {
    # Evaluate each original predicate once, in the same short-circuit order.
    # Retain only the first refused budget, including through reader cleanup.
    $cause = if ($script:HostWatch.Elapsed.TotalSeconds -gt $script:HostLimits.Seconds) {
        'deadline'
    } elseif ($script:HostCount -gt $script:HostLimits.Entries) {
        'entries'
    } elseif ($script:HostBytes -gt $script:HostLimits.TotalBytes) {
        'bytes'
    } elseif ($script:HostHandles.Count -gt $script:HostLimits.Handles) {
        'handles'
    } elseif ($script:HostMetadataBytes -gt $script:HostLimits.MetadataBytes) {
        'metadata'
    } else { return }
    if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = $cause }
    throw 'limit'
}

function Get-CanonicalHostPath([string] $Value) {
    if ([string]::IsNullOrEmpty($Value) -or $Value.Length -gt 1024 -or
        $Value -notmatch '^[A-Za-z]:\\' -or $Value.Substring(2).Contains(':') -or
        $Value.Contains('/') -or $Value -match '[\x00-\x1f*?"<>|]') {
        # Classify only an already-refused immutable string, in the exact
        # original short-circuit order. These labels grant no path allowance.
        if ([string]::IsNullOrEmpty($Value)) { throw 'path_empty' }
        if ($Value.Length -gt 1024) { throw 'path_length' }
        if ($Value -notmatch '^[A-Za-z]:\\') { throw 'path_drive' }
        if ($Value.Substring(2).Contains(':')) { throw 'path_colon' }
        if ($Value.Contains('/')) { throw 'path_slash' }
        if ($Value -match '[\x00-\x1f*?"<>|]') { throw 'path_chars' }
        throw 'path_input'
    }
    $full = [IO.Path]::GetFullPath($Value)
    if ($full.TrimEnd('\') -cne $Value.TrimEnd('\')) { throw 'path_normalized' }
    foreach ($part in $Value.Substring(3).Split('\')) {
        if ($part -eq '' -and $Value.Length -eq 3) { continue }
        if ($part -eq '' -or $part -in @('.', '..') -or $part.EndsWith('.') -or $part.EndsWith(' ') -or
            $part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)' -or $part -ieq '.git') {
            if ($part -eq '') { throw 'path_part_empty' }
            if ($part -in @('.', '..')) { throw 'path_part_dot' }
            if ($part.EndsWith('.')) { throw 'path_part_trailingdot' }
            if ($part.EndsWith(' ')) { throw 'path_part_trailingspace' }
            if ($part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)') { throw 'path_part_device' }
            if ($part -ieq '.git') { throw 'path_part_git' }
            throw 'path_component'
        }
    }
    return $full
}

# Internal reader callers initialize the owner first. The original lexical
# helper above remains available before initialization. After Close, a null
# owner falls back to full validation without retaining any result.
function Get-HostCanonicalPath([string] $Value) {
    $memo = $script:HostCanonicalPaths
    [string] $full = ''
    if ($null -ne $memo -and -not [string]::IsNullOrEmpty($Value) -and
        $memo.TryGetValue($Value, [ref]$full)) { return $full }
    if ($null -eq $script:HostCanonicalInvoker) { $full = Get-CanonicalHostPath $Value }
    else {
        $validated = $script:HostCanonicalInvoker.Invoke($Value)
        switch ($validated.Item2) {
            0 { $full = $validated.Item1; break }
            -1 { $full = Get-CanonicalHostPath $Value; break }
            1 { throw 'path_empty' }
            2 { throw 'path_length' }
            3 { throw 'path_drive' }
            4 { throw 'path_colon' }
            5 { throw 'path_slash' }
            6 { throw 'path_chars' }
            7 { throw 'path_normalized' }
            8 { throw 'path_part_empty' }
            9 { throw 'path_part_dot' }
            10 { throw 'path_part_trailingdot' }
            11 { throw 'path_part_trailingspace' }
            12 { throw 'path_part_device' }
            13 { throw 'path_part_git' }
            default { throw 'path_input' }
        }
    }
    if ($null -ne $memo) {
        $units = $Value.Length + $full.Length
        if ($memo.Count -lt $script:HostCanonicalEntryLimit -and
            $units -le $script:HostCanonicalUnitLimit - $script:HostCanonicalUnits) {
            $memo.Add($Value, $full)
            $script:HostCanonicalUnits += $units
        }
    }
    return $full
}

function Get-HostHandleInfo([IntPtr] $Handle, $Expected, [bool] $Content = $false) {
    # These synchronous calls have no callbacks. Never overlap use or return a
    # view of shared scratch: the result below contains only copied values.
    if ($script:HostInfoBusy -or $script:HostInfoMemory -eq [IntPtr]::Zero) { throw 'identity' }
    $script:HostInfoBusy = $true
    try {
        $hasExpected = $PSBoundParameters.ContainsKey('Expected')
        if ($hasExpected -and $null -ne $script:HostComparisonInvoker -and
            [object]::ReferenceEquals($script:HostType, $script:HostComparisonType) -and
            [object]::ReferenceEquals($script:HostInfo, $script:HostComparisonInfo)) {
            $status = $script:HostComparisonInvoker.Invoke($script:HostType, $script:HostInfo,
                $Handle, $script:HostInfoMemory, $script:HostInfoScratch, $Expected, $Content)
            switch ($status) {
                0 { return }
                1 { throw 'type' }
                2 { throw 'identity' }
                3 { throw 'reparse' }
                4 { throw 'changed' }
                default { throw 'identity' }
            }
        }
        if (-not $hasExpected -and $script:HostInitialInvoker -is [Delegate] -and
            [object]::ReferenceEquals($script:HostInitialInvoker, $script:HostInitialInvokerOwner) -and
            [object]::ReferenceEquals($script:HostType, $script:HostComparisonType) -and
            [object]::ReferenceEquals($script:HostInfo, $script:HostComparisonInfo)) {
            $initial = $script:HostInitialInvoker.Invoke($script:HostType, $script:HostInfo,
                $Handle, $script:HostInfoMemory, $script:HostInfoScratch)
            if ($initial -is [Collections.Hashtable]) {
                if ($script:HostManagedInitialInfos -lt 200000) { $script:HostManagedInitialInfos++ }
                return $initial
            }
            switch ($initial) {
                1 { throw 'type' }
                2 { throw 'identity' }
                3 { throw 'reparse' }
                4 { break } # Getter/method probe: no native call occurred yet.
                default { throw 'identity' }
            }
        }
        if ($script:HostType.Invoke($Handle) -ne 1) { throw 'type' }
        $memory = $script:HostInfoMemory
        if ($script:HostInfo.Invoke($Handle, $memory) -eq 0) { throw 'identity' }
        $data = $script:HostInfoScratch; [Runtime.InteropServices.Marshal]::Copy($memory, $data, 0, 52)
        $attributes = [BitConverter]::ToUInt32($data, 0)
        if (($attributes -band 0x400) -ne 0) { throw 'reparse' }
        if ($PSBoundParameters.ContainsKey('Expected')) {
            if ($Expected -isnot [byte[]] -or $Expected.Length -ne 52) { throw 'identity' }
            # Equality of these fixed-width spans is exactly the old copied
            # attributes/volume/file-index and optional size/write/link check.
            # No hashtable or hex strings are needed for a repeated comparison.
            if ($attributes -ne [BitConverter]::ToUInt32($Expected, 0) -or
                [BitConverter]::ToUInt32($data, 28) -ne [BitConverter]::ToUInt32($Expected, 28) -or
                [BitConverter]::ToUInt64($data, 44) -ne [BitConverter]::ToUInt64($Expected, 44) -or
                ($Content -and (
                    [BitConverter]::ToUInt64($data, 32) -ne [BitConverter]::ToUInt64($Expected, 32) -or
                    [BitConverter]::ToUInt64($data, 20) -ne [BitConverter]::ToUInt64($Expected, 20) -or
                    [BitConverter]::ToUInt32($data, 40) -ne [BitConverter]::ToUInt32($Expected, 40)))) { throw 'changed' }
            return
        }
        return @{ attributes = $attributes; directory = (($attributes -band 0x10) -ne 0);
            bytes = ([uint64][BitConverter]::ToUInt32($data, 32) * 4294967296 + [uint64][BitConverter]::ToUInt32($data, 36));
            write = [Convert]::ToHexString($data, 20, 8); links = [BitConverter]::ToUInt32($data, 40);
            identity = ([Convert]::ToHexString($data, 28, 4) + [Convert]::ToHexString($data, 44, 8)) }
    } finally { $script:HostInfoBusy = $false }
}

function Open-HostReadHandle([string] $LiteralPath, [bool] $Directory) {
    Assert-HostBudget
    $full = Get-HostCanonicalPath $LiteralPath
    $wide = [Runtime.InteropServices.Marshal]::StringToHGlobalUni('\\?\' + $full)
    try {
        # OPEN_REPARSE_POINT always; BACKUP_SEMANTICS only enables directory
        # opens. Share READ only prevents original opened files being written,
        # renamed or deleted until the complete snapshot has been checked.
        $access = if ($Directory) { [uint32]128 } else { [uint32]2147483648 }
        # Full forward payload reads: cache hint only; no-follow stays set.
        $flags = if ($Directory) { [uint32]0x02200000 } else { [uint32]0x08200000 }
        $handle = $script:HostOpen.Invoke($wide, $access, [uint32]1, [IntPtr]::Zero, [uint32]3, $flags, [IntPtr]::Zero)
    } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($wide) }
    if ($handle -eq [IntPtr]::new(-1)) { throw 'open' }
    $script:HostHandles.Add($handle); Assert-HostBudget
    $info = Get-HostHandleInfo $handle
    if ($info.directory -ne $Directory) { throw 'type' }
    # Private record storage, never a receipt field or a shared-scratch view.
    # At most the existing 200256 retained handles own one 52-byte copy each.
    $comparison = [byte[]]$script:HostInfoScratch.Clone()
    return @{ path = $full; handle = $handle; initial = $info; comparison = $comparison }
}

function Assert-HostIdentity($Record, [bool] $Content) {
    Get-HostHandleInfo $Record.handle $Record.comparison $Content
}

function Open-HostDirectoryChain([string] $LiteralPath) {
    $full = Get-HostCanonicalPath $LiteralPath
    $cached = $script:HostLastDirectoryChain
    if ($null -ne $cached -and $cached.Length -le $script:HostDirectoryChainLimit -and
        [string]::Equals($full, $script:HostLastDirectoryPath, [StringComparison]::Ordinal)) {
        # Same records, ROOT to LEAF, with every original native type/identity
        # read. Only prefix splitting, path allocation and dictionary probes
        # are avoided; no filesystem fact or successful check is reused.
        $resume = 0
        if ($cached.Length -gt 0 -and $cached.Length -le 512 -and
            $script:HostChainInvoker -is [Delegate] -and
            [object]::ReferenceEquals($script:HostChainInvoker, $script:HostChainInvokerOwner) -and
            [object]::ReferenceEquals($script:HostComparisonInvoker, $script:HostChainComparison) -and
            [object]::ReferenceEquals($script:HostType, $script:HostComparisonType) -and
            [object]::ReferenceEquals($script:HostInfo, $script:HostComparisonInfo) -and
            -not $script:HostInfoBusy -and $script:HostInfoMemory -ne [IntPtr]::Zero) {
            # The exact native calls have no callbacks, so this one sequential
            # scratch ownership covers the whole loop. Probes and unavailable
            # scratch use the original per-entry guard and adaptation below.
            $script:HostInfoBusy = $true
            try {
                $status = $script:HostChainInvoker.Invoke($cached, $script:HostComparisonInvoker,
                    $script:HostType, $script:HostInfo, $script:HostInfoMemory, $script:HostInfoScratch)
                switch ($status) {
                    0 { $resume = $cached.Length; break }
                    1 { throw 'type' }
                    2 { throw 'identity' }
                    3 { throw 'reparse' }
                    4 { throw 'changed' }
                    default {
                        if ($status -ge 0) { throw 'identity' }
                        $resume = -1 - $status
                    }
                }
            } finally { $script:HostInfoBusy = $false }
        }
        # A record needing PowerShell adaptation resumes at its first unread
        # entry. Earlier real native reads are never skipped or repeated.
        for ($i = $resume; $i -lt $cached.Length; $i++) {
            $record = $cached[$i]
            Get-HostHandleInfo $record.handle $record.comparison $false
        }
        return $cached[$cached.Length - 1]
    }
    # Never publish an incomplete chain after a failed miss. A full cache is
    # an optimization miss, not a new path refusal or an omitted validation.
    $script:HostLastDirectoryPath = $null
    $script:HostLastDirectoryChain = $null
    $chain = [Collections.Generic.List[object]]::new()
    $remember = $true
    $current = $full.Substring(0, 3)
    $parts = @('') + @($full.Substring(3).Split('\') | Where-Object { $_ -ne '' })
    foreach ($part in $parts) {
        if ($part -ne '') { $current = [IO.Path]::Combine($current, $part) }
        if (-not $script:HostDirectories.ContainsKey($current)) {
            $record = Open-HostReadHandle $current $true
            $script:HostDirectories.Add($current, $record)
        } else {
            $record = $script:HostDirectories[$current]
            Get-HostHandleInfo $record.handle $record.comparison $false
        }
        if ($remember) {
            if ($chain.Count -lt $script:HostDirectoryChainLimit) { $chain.Add($record) }
            else { $remember = $false; $chain.Clear() }
        }
    }
    if ($remember) {
        $script:HostLastDirectoryPath = $full
        $script:HostLastDirectoryChain = @($chain.ToArray())
    }
    return $script:HostDirectories[$full]
}

function Get-HostFileFact([string] $LiteralPath, [int] $TextLimit = 0, [uint64] $RemainingBytes = [uint64]::MaxValue) {
    $script:HostPreflightWork = 'file_open'
    $full = Get-HostCanonicalPath $LiteralPath
    $null = Open-HostDirectoryChain ([IO.Path]::GetDirectoryName($full))
    $record = Open-HostReadHandle $full $false
    $script:HostFileRecords.Add($record)
    if ($record.initial.bytes -gt $script:HostLimits.FileBytes) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'file_size'
    }
    if ($TextLimit -gt 0 -and $record.initial.bytes -gt $TextLimit) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'text_size'
    }
    # The retained no-follow handle supplies the size. Refuse before creating
    # a stream or digest so the next payload cannot exceed the root budget.
    if ($record.initial.bytes -gt $RemainingBytes) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'limit'
    }
    $safe = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($record.handle, $false)
    $stream = [IO.FileStream]::new($safe, [IO.FileAccess]::Read, 1048576, $false)
    $digest = [Security.Cryptography.SHA256]::Create(); $buffer = $script:HostBuffer
    $text = if ($TextLimit -gt 0) { [IO.MemoryStream]::new() } else { $null }
    try {
        $script:HostPreflightWork = 'file_hash'
        $limits = $script:HostLimits
        $managed = $false
        if ($script:HostHashAdmissionInvoker -is [Delegate] -and
            [object]::ReferenceEquals($script:HostHashAdmissionInvoker, $script:HostHashAdmissionOwner)) {
            # Fixed twenty-slot state, no member Value/property evaluation.
            $admissionState = [object[]]@($script:HostHashInvoker, $script:HostHashInvokerOwner,
                ${function:Assert-HostBudget}, $script:HostHashBudgetOwner,
                ${function:Open-HostReadHandle}, $script:HostHashOpenOwner,
                ${function:Get-HostHandleInfo}, $script:HostHashInfoOwner,
                $limits, $script:HostHashLimitsOwner, $script:HostHandles, $script:HostHashHandlesOwner,
                $script:HostWatch, $buffer, $script:HostCount, $script:HostBytes,
                $script:HostMetadataBytes, $stream, $digest, $text)
            $managed = $script:HostHashAdmissionInvoker.Invoke($admissionState)
            if ($managed -and $script:HostManagedAdmissions -lt 200000) { $script:HostManagedAdmissions++ }
        }
        if (-not $managed) {
            $managed = $script:HostHashInvoker -is [Delegate] -and
                [object]::ReferenceEquals($script:HostHashInvoker, $script:HostHashInvokerOwner) -and
                [object]::ReferenceEquals(${function:Assert-HostBudget}, $script:HostHashBudgetOwner) -and
                [object]::ReferenceEquals(${function:Open-HostReadHandle}, $script:HostHashOpenOwner) -and
                [object]::ReferenceEquals(${function:Get-HostHandleInfo}, $script:HostHashInfoOwner) -and
                [object]::ReferenceEquals($limits, $script:HostHashLimitsOwner) -and
                [object]::ReferenceEquals($script:HostHandles, $script:HostHashHandlesOwner) -and
                $script:HostWatch -is [Diagnostics.Stopwatch] -and
                $buffer -is [byte[]] -and $script:HostHandles -is [Collections.Generic.List[IntPtr]] -and
                $script:HostCount -is [int] -and $script:HostBytes -is [uint64] -and
                $script:HostMetadataBytes -is [int]
            # A fixed owner has plain numeric limits. ETS shadows or a probe's
            # alternative number/clock semantics retain the original loop below.
            if ($managed) {
                $limitMembers = $limits.PSObject.Members
                foreach ($key in @('Seconds', 'Entries', 'TotalBytes', 'Handles', 'MetadataBytes')) {
                    if ($null -ne $limitMembers[$key]) { $managed = $false; break }
                }
                $clockMember = $script:HostWatch.PSObject.Members['Elapsed']
                $secondsMember = ([TimeSpan]::Zero).PSObject.Members['TotalSeconds']
                $bufferMember = $buffer.PSObject.Members['Length']
                $handlesMember = $script:HostHandles.PSObject.Members['Count']
                $readMember = $stream.PSObject.Members['Read']
                $digestMembers = $digest.PSObject.Members
                $blockMember = $digestMembers['TransformBlock']; $finalMember = $digestMembers['TransformFinalBlock']
                $writeOrdinary = $null -eq $text -or
                    $text.PSObject.Members['Write'] -is [Management.Automation.PSMethod]
                if ($readMember -isnot [Management.Automation.PSMethod] -or
                    $blockMember -isnot [Management.Automation.PSMethod] -or
                    $finalMember -isnot [Management.Automation.PSMethod] -or -not $writeOrdinary -or
                    $clockMember -isnot [Management.Automation.PSProperty] -or
                    $secondsMember -isnot [Management.Automation.PSProperty] -or
                    $bufferMember -isnot [Management.Automation.PSProperty] -or
                    $handlesMember -isnot [Management.Automation.PSProperty] -or
                    $limits['Seconds'] -isnot [int] -or $limits['Entries'] -isnot [int] -or
                    $limits['TotalBytes'] -isnot [long] -or $limits['TotalBytes'] -lt 0 -or
                    $limits['Handles'] -isnot [int] -or $limits['MetadataBytes'] -isnot [int]) { $managed = $false }
            }
        }
        $size = [uint64]0
        if ($managed) {
            # No callbacks occur in this owned FileStream/SHA256 loop. These
            # counters cannot change until the same synchronous file completes.
            $budget = [long[]]@($limits['Seconds'], $script:HostCount, $limits['Entries'],
                $script:HostHandles.Count, $limits['Handles'], $script:HostMetadataBytes, $limits['MetadataBytes'])
            $hashSize = [uint64[]]::new(1)
            $status = $script:HostHashInvoker.Invoke($stream, $digest, $buffer, $text,
                $record, $script:HostWatch, $budget,
                $script:HostBytes, [uint64]$limits['TotalBytes'], $hashSize)
            switch ($status) {
                0 {
                    $size = $hashSize[0]
                    if ($script:HostManagedHashes -lt 200000) { $script:HostManagedHashes++ }
                    break
                }
                1 { if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'deadline' }; throw 'limit' }
                2 { if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'entries' }; throw 'limit' }
                3 { if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }; throw 'limit' }
                4 { if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'handles' }; throw 'limit' }
                5 { if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'metadata' }; throw 'limit' }
                6 { throw 'changed' }
                7 { $managed = $false; break }
                default { throw 'identity' }
            }
        }
        if (-not $managed) {
            while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                Assert-HostBudget; $size += $count
                if ($size -gt $record.initial.bytes) { throw 'changed' }
                $null = $digest.TransformBlock($buffer, 0, $count, $null, 0)
                if ($null -ne $text) { $text.Write($buffer, 0, $count) }
            }
            $null = $digest.TransformFinalBlock([byte[]]::new(0), 0, 0)
            if ($size -ne $record.initial.bytes) { throw 'changed' }
        }
        $script:HostPreflightWork = 'file_identity'
        Get-HostHandleInfo $record.handle $record.comparison $true
        $fact = @{ bytes = $size; sha256 = [Convert]::ToHexString($digest.Hash).ToLowerInvariant() }
        if ($null -ne $text) { $fact.text = [Text.UTF8Encoding]::new($false, $true).GetString($text.ToArray()) }
        return $fact
    } finally {
        $stream.Dispose(); $safe.Dispose(); $digest.Dispose()
        if ($null -ne $text) { $text.Dispose() }
    }
}

function Get-HostDirectoryNames([string] $Directory) {
    $names = [Collections.Generic.List[string]]::new()
    $folded = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Directory)) {
        Assert-HostBudget
        $name = [IO.Path]::GetFileName($path)
        $null = Get-HostCanonicalPath ([IO.Path]::Combine($Directory, $name))
        if (-not $folded.Add($name)) { throw 'alias' }
        $names.Add($name)
        if ($names.Count -gt $script:HostLimits.Entries) {
            if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'entries' }
            throw 'entries'
        }
    }
    $result = $names.ToArray(); [Array]::Sort($result, [StringComparer]::Ordinal)
    return ,$result
}

function Get-HostRootInventory([string] $LiteralRoot) {
    $root = Get-HostCanonicalPath $LiteralRoot
    $script:HostPreflightWork = 'directory_open'
    $null = Open-HostDirectoryChain $root
    $pending = [Collections.Generic.Stack[object]]::new(); $pending.Push(@{ path = $root; depth = 0 })
    $snapshots = [Collections.Generic.List[object]]::new(); $files = [Collections.Generic.List[object]]::new()
    while ($pending.Count -gt 0) {
        Assert-HostBudget; $next = $pending.Pop()
        if ($next.depth -gt $script:HostLimits.Depth) { throw 'depth' }
        $script:HostPreflightWork = 'directory_open'
        $directory = Open-HostDirectoryChain $next.path
        $script:HostPreflightWork = 'enumeration'
        $names = Get-HostDirectoryNames $next.path
        $snapshots.Add(@{ directory = $directory; names = $names })
        foreach ($name in $names) {
            $script:HostCount++; Assert-HostBudget
            $path = [IO.Path]::Combine($next.path, $name)
            # Attributes only select the read-open kind. The final no-follow
            # handle supplies the authoritative type before any payload read.
            $script:HostPreflightWork = 'entry_type'
            $attributes = [IO.File]::GetAttributes($path)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'reparse' }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) {
                $script:HostPreflightWork = 'directory_open'
                $null = Open-HostDirectoryChain $path
                $pending.Push(@{ path = $path; depth = $next.depth + 1 })
            } else {
                $fact = Get-HostFileFact $path 0 ([uint64]($script:HostLimits.TotalBytes - $script:HostBytes))
                $script:HostPreflightWork = 'receipt_row'
                $script:HostBytes += $fact.bytes; Assert-HostBudget
                $relative = [IO.Path]::GetRelativePath($root, $path).Replace('\', '/')
                # JSON upper bound before adding a row, including worst-case
                # escaping. Leave 2MiB for fixed roots/roles and top-level data.
                $script:HostMetadataBytes += 160 + 6 * $relative.Length
                Assert-HostBudget
                $files.Add(@{ path = $relative; bytes = $fact.bytes; sha256 = $fact.sha256 })
            }
        }
    }
    foreach ($snapshot in $snapshots) {
        $script:HostPreflightWork = 'snapshot_identity'
        Assert-HostBudget; Get-HostHandleInfo $snapshot.directory.handle $snapshot.directory.comparison $true
        $script:HostPreflightWork = 'snapshot_names'
        $again = Get-HostDirectoryNames $snapshot.directory.path
        if ($again.Count -ne $snapshot.names.Count) { throw 'changed' }
        for ($i = 0; $i -lt $again.Count; $i++) { if ($again[$i] -cne $snapshot.names[$i]) { throw 'changed' } }
    }
    return @{ root = $root; files = @($files.ToArray()) }
}

function Close-HostReader {
    if ($script:HostInfoBusy) { throw 'identity' }
    # Refuse an active borrowed reader before closing any handle or freeing
    # scratch. Retirement latches this owner closed against new admissions.
    if ($script:HostTraversalCompilerReady) {
        [Morpheus.OwnedTraversal20261028.Reader]::RetireOwner($script:HostTraversalOwners)
    }
    $good = $true
    try {
        if ($null -ne $script:HostHandles) {
            for ($i = $script:HostHandles.Count - 1; $i -ge 0; $i--) {
                $handle = $script:HostHandles[$i]
                # An uncertain close is never retried on a potentially reused handle.
                $script:HostHandles[$i] = [IntPtr]::Zero
                if ($handle -ne [IntPtr]::Zero -and $script:HostClose.Invoke($handle) -eq 0) { $good = $false }
            }
            $script:HostHandles.Clear()
        }
    } finally {
        try {
            $script:HostTraversalReader = $null
            $script:HostTraversalCompilerReady = $false
            $script:HostTraversalOwners = $null
            $script:HostTraversalMemoryOwner = [IntPtr]::Zero
            $script:HostTraversalKernelOwner = [IntPtr]::Zero
            # Owned scratch is freed even when closing a native file handle fails.
            $memory = $script:HostInfoMemory
            $script:HostInfoMemory = [IntPtr]::Zero
            $script:HostInfoScratch = $null
            $script:HostInfoBusy = $false
            $script:HostComparisonInvoker = $null
            $script:HostComparisonType = $null
            $script:HostComparisonInfo = $null
            $script:HostInitialInvoker = $null
            $script:HostInitialInvokerOwner = $null
            $script:HostHashAdmissionInvoker = $null
            $script:HostHashAdmissionOwner = $null
            $script:HostChainInvoker = $null
            $script:HostChainInvokerOwner = $null
            $script:HostChainComparison = $null
            $script:HostHashInvoker = $null
            $script:HostHashInvokerOwner = $null
            $script:HostHashBudgetOwner = $null
            $script:HostHashLimitsOwner = $null
            $script:HostHashHandlesOwner = $null
            $script:HostHashOpenOwner = $null
            $script:HostHashInfoOwner = $null
            if ($memory -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($memory) }
        } finally {
            # Retire lexical ownership even if native cleanup throws. No result
            # survives Close or is shared with the next initialized reader.
            $chain = $script:HostLastDirectoryChain
            $script:HostLastDirectoryPath = $null
            $script:HostLastDirectoryChain = $null
            if ($null -ne $chain) { [Array]::Clear($chain, 0, $chain.Length) }
            $memo = $script:HostCanonicalPaths
            $script:HostCanonicalPaths = $null
            $script:HostCanonicalInvoker = $null
            $script:HostCanonicalUnits = 0
            if ($null -ne $memo) { $memo.Clear() }
        }
    }
    if (-not $good) { throw 'handle_cleanup' }
}

function Get-HostReceiptMetadata([hashtable] $Values) {
    $required = @('run_id', 'run_attempt', 'source_commit', 'image_os', 'image_version', 'powershell_version')
    if ($Values.Count -ne $required.Count) { throw 'metadata' }
    foreach ($key in $required) {
        if (-not $Values.ContainsKey($key) -or $Values[$key] -isnot [string]) { throw 'metadata' }
    }
    if ($Values.run_id -cnotmatch '^[0-9]{1,20}$' -or $Values.run_attempt -cnotmatch '^[0-9]{1,6}$' -or
        $Values.source_commit -cnotmatch '^[0-9a-f]{40}$') { throw 'metadata' }
    foreach ($key in @('image_os', 'image_version', 'powershell_version')) {
        if ($Values[$key] -cnotmatch '^[A-Za-z0-9._-]{1,64}$') { throw 'metadata' }
    }
    return $Values
}

function Get-HostPreflightFailureMessage($Stage, $BudgetCause) {
    # Keep the original one-argument result byte-for-byte. The entry opts into
    # a separate closed budget label; never stringify an untrusted value.
    $budgetSuffix = ''
    if ($PSBoundParameters.ContainsKey('BudgetCause')) {
        $budgetSuffix = ' budget=unknown'
        if ($BudgetCause -is [string]) {
            foreach ($budget in @('deadline', 'entries', 'bytes', 'handles', 'metadata')) {
                if ([string]::Equals($BudgetCause, $budget, [StringComparison]::Ordinal)) {
                    $budgetSuffix = ' budget=' + $budget
                    break
                }
            }
        }
    }
    # Only source-fixed phases can reach the log. Never inspect the exception,
    # a path, environment value or another object's string representation.
    if ($Stage -is [string]) {
        foreach ($phase in @('initialization', 'bootstrap', 'vs_inventory',
            'sdk_inventory', 'role_binding', 'final_identity', 'serialization')) {
            if ([string]::Equals($Stage, $phase, [StringComparison]::Ordinal)) {
                return 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=' + $phase + $budgetSuffix
            }
        }
    }
    return 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=unknown' + $budgetSuffix
}

# Export only fixed work labels and bounded integers after an actual inventory
# refusal. No path, payload, exception, arbitrary object or elapsed time escapes.
function Get-HostPreflightProgressMessage($Work, $Entries, $Bytes, $ManagedHashes) {
    $phase = 'unknown'
    if ($Work -is [string]) {
        foreach ($known in @('idle', 'directory_open', 'enumeration', 'entry_type',
            'file_open', 'file_hash', 'file_identity', 'receipt_row', 'snapshot_identity',
            'snapshot_names', 'final_directory', 'final_file', 'json')) {
            if ([string]::Equals($Work, $known, [StringComparison]::Ordinal)) { $phase = $known; break }
        }
    }
    $count = 0; $size = [uint64]0; $hashes = 0
    # Include the first over-limit entry only, never arbitrary counter values.
    if ($Entries -is [int] -and $Entries -ge 0 -and $Entries -le 200001) { $count = $Entries }
    if ($Bytes -is [uint64] -and $Bytes -le [uint64]68719476736) { $size = $Bytes }
    if ($ManagedHashes -is [int] -and $ManagedHashes -ge 0 -and $ManagedHashes -le 200000) { $hashes = $ManagedHashes }
    return ' work=' + $phase + ' entries=' + [Convert]::ToString([int]$count, [Globalization.CultureInfo]::InvariantCulture) +
        ' bytes=' + [Convert]::ToString([uint64]$size, [Globalization.CultureInfo]::InvariantCulture) +
        ' managed_hashes=' + [Convert]::ToString([int]$hashes, [Globalization.CultureInfo]::InvariantCulture)
}

function Get-HostToolchainInventory {
    $script:HostPreflightStage = 'initialization'
    Initialize-HostReader
    try {
        $owned = New-HostOwnedTraversal
        if ($null -ne $owned) { return (Invoke-HostOwnedTraversal $owned '') }
        $script:HostPreflightStage = 'role_binding'
        $drive = $env:SystemRoot.Substring(0, 2)
        $vsBase = $drive + '\Program Files\Microsoft Visual Studio\2022'
        $candidates = @('Enterprise', 'Professional', 'Community', 'Preview', 'BuildTools') |
            ForEach-Object { [IO.Path]::Combine($vsBase, $_) } | Where-Object { [IO.Directory]::Exists($_) }
        if (@($candidates).Count -ne 1) { throw 'visual_studio_selection' }
        $vs = Get-HostCanonicalPath (@($candidates)[0])
        $sdk = $drive + '\Program Files (x86)\Windows Kits\10'
        $versionFile = [IO.Path]::Combine($vs, 'VC', 'Auxiliary', 'Build', 'Microsoft.VCToolsVersion.default.txt')
        $versionFact = Get-HostFileFact $versionFile 256
        $version = $versionFact.text.Trim()
        if ($version -notmatch '^14\.[0-9]{1,3}\.[0-9]{1,6}$') { throw 'msvc_version' }
        $msvc = [IO.Path]::Combine($vs, 'VC', 'Tools', 'MSVC', $version)
        $vcvars = [IO.Path]::Combine($vs, 'VC', 'vcvarsall.bat')
        if (-not [IO.File]::Exists($vcvars)) { $vcvars = [IO.Path]::Combine($vs, 'VC', 'Auxiliary', 'Build', 'vcvarsall.bat') }
        $roles = [ordered]@{ vcvarsall = $vcvars; msvc_default_version = $versionFile;
            cl = [IO.Path]::Combine($msvc, 'bin', 'Hostx64', 'x64', 'cl.exe');
            link = [IO.Path]::Combine($msvc, 'bin', 'Hostx64', 'x64', 'link.exe');
            msvc_include = [IO.Path]::Combine($msvc, 'include', 'vcruntime.h');
            msvc_lib = [IO.Path]::Combine($msvc, 'lib', 'x64', 'vcruntime.lib');
            sdk_rc = [IO.Path]::Combine($sdk, 'bin', '10.0.26100.0', 'x64', 'rc.exe');
            sdk_mt = [IO.Path]::Combine($sdk, 'bin', '10.0.26100.0', 'x64', 'mt.exe');
            sdk_um = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'um', 'Windows.h');
            sdk_shared = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'shared', 'sdkddkver.h');
            sdk_ucrt = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'ucrt', 'stdlib.h');
            sdk_kernel_lib = [IO.Path]::Combine($sdk, 'Lib', '10.0.26100.0', 'um', 'x64', 'kernel32.lib');
            sdk_ucrt_lib = [IO.Path]::Combine($sdk, 'Lib', '10.0.26100.0', 'ucrt', 'x64', 'ucrt.lib') }
        $roleFacts = [ordered]@{}
        foreach ($name in $roles.Keys) {
            $fact = Get-HostFileFact $roles[$name]
            if ($fact.bytes -le 0) { throw 'role' }
            $roleFacts[$name] = @{ path = $roles[$name]; fact = $fact }
        }
        $script:HostPreflightStage = 'vs_inventory'
        $vsInventory = Get-HostRootInventory $vs
        $script:HostPreflightStage = 'sdk_inventory'
        $sdkInventory = Get-HostRootInventory $sdk
        $roots = @($vsInventory, $sdkInventory)
        $script:HostPreflightStage = 'final_identity'
        $script:HostPreflightWork = 'final_directory'
        foreach ($record in $script:HostDirectories.Values) { Get-HostHandleInfo $record.handle $record.comparison $false }
        $script:HostPreflightWork = 'final_file'
        foreach ($record in $script:HostFileRecords) { Assert-HostBudget; Get-HostHandleInfo $record.handle $record.comparison $true }
        Assert-HostBudget
        $inventory = [ordered]@{ schema = 'morpheus-native-windows-host-toolchain-inventory-v1';
            visual_studio_root = $vs; visual_studio_year = '2022'; msvc_toolset_version = $version;
            windows_sdk_version = '10.0.26100.0'; roots = $roots; roles = $roleFacts;
            upstream_build_revision = 'be1a8f6dcd7df7e46320192c5e2f364e50d79bbf';
            upstream_vs_toolchain_sha256 = 'd13a7b17b81c2084de5a76e907c458ca997a4ddcdef96d3b74c8fcb3aca5f463';
            upstream_setup_toolchain_sha256 = 'f6ae24c99ae0bfc72b093da415615119652f25415155dffb4db22f49b78ca8cf';
            entries = $script:HostCount; total_bytes = $script:HostBytes; limits = $script:HostLimits;
            environment_realized = $false; vcvars_or_compilers_executed = $false;
            host_approval_granted = $false; build_ready = $false; distribution_ready = $false }
        return (Set-HostInventoryImplementation $inventory 'powershell-original-fallback-v1')
    } finally { Close-HostReader }
}

# Read-only fresh-host identity collection. No Python/Git/tool is executed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$script:HostPreflightStage = 'bootstrap'
$script:HostPreflightBudgetCause = 'unknown'
try {
    if ($Mode -cnotin @('basic', 'host-toolchain')) { throw 'mode' }
    if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
        $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'X64' -or
        $env:GITHUB_REPOSITORY -ne 'brk275/Morpheus' -or $env:GITHUB_REF -ne 'refs/heads/main' -or
        $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch' -or $env:GITHUB_ACTOR -ne 'brk275' -or
        $env:GITHUB_TRIGGERING_ACTOR -ne 'brk275') { throw 'guard' }
    function Get-BoundedFileIdentity([string] $LiteralPath) {
        $full = [IO.Path]::GetFullPath($LiteralPath)
        if ($full -notmatch '^[A-Za-z]:\\' -or $full.StartsWith('\\') -or
            $full.Substring(2).Contains(':')) { throw 'path' }
        $item = Get-Item -LiteralPath $full -Force
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            $item.Length -gt 268435456 -or $item.Length -le 0) { throw 'file' }
        $parent = $item.Directory
        while ($null -ne $parent) {
            if ($parent.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'parent' }
            $parent = $parent.Parent
        }
        # Share READ only: no concurrent writer/deletion through this open.
        $stream = [IO.File]::Open($full, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try {
            if ($stream.Length -ne $item.Length) { throw 'changed' }
            $digest = [Security.Cryptography.SHA256]::Create()
            try { $sha = [Convert]::ToHexString($digest.ComputeHash($stream)).ToLowerInvariant() }
            finally { $digest.Dispose() }
            if ($stream.Length -ne $item.Length) { throw 'changed' }
        } finally { $stream.Dispose() }
        $current = Get-Item -LiteralPath $full -Force
        if ($current.Length -ne $item.Length -or $current.LastWriteTimeUtc -ne $item.LastWriteTimeUtc -or
            ($current.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'changed' }
        return @{ path = $full; bytes = $item.Length; sha256 = $sha }
    }
    $facts = @{}
    foreach ($name in @('python.exe', 'git.exe')) {
        # Capture the first genuine application selected by the fresh hosted
        # image, with no alias/function or fallback executable invocation.
        $commands = @(Get-Command -Name $name -CommandType Application -All -ErrorAction SilentlyContinue)
        if ($commands.Count -gt 0) { $facts[$name] = Get-BoundedFileIdentity $commands[0].Source }
        else { $facts[$name] = $null }
    }
    if ($Mode -eq 'host-toolchain') {
        $script:HostPreflightStage = 'initialization'
        $metadata = Get-HostReceiptMetadata @{ run_id = $env:GITHUB_RUN_ID; run_attempt = $env:GITHUB_RUN_ATTEMPT;
            source_commit = $env:GITHUB_SHA; image_os = $env:ImageOS; image_version = $env:ImageVersion;
            powershell_version = $PSVersionTable.PSVersion.ToString() }
        $inventory = Get-HostToolchainInventory
        $script:HostPreflightStage = 'serialization'
        foreach ($key in $metadata.Keys) { $inventory[$key] = $metadata[$key] }
        $inventory.bootstrap_tools = $facts
        # Only filenames, sizes and hashes of fixed host roots leave the reader.
        # Never print SDK/tool bytes, ambient environment or native error text.
        $script:HostPreflightWork = 'json'
        $json = $inventory | ConvertTo-Json -Depth 10 -Compress
        if ([Text.Encoding]::UTF8.GetByteCount($json) -gt 33554432) {
            if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'metadata' }
            throw 'json_size'
        }
        Write-Output $json
        return
    }
    $script:HostPreflightStage = 'serialization'
    [ordered]@{
        schema = 'morpheus-native-windows-host-preflight-v1'
        run_id = $env:GITHUB_RUN_ID
        run_attempt = $env:GITHUB_RUN_ATTEMPT
        source_commit = $env:GITHUB_SHA
        image_os = $env:ImageOS
        image_version = $env:ImageVersion
        powershell_version = $PSVersionTable.PSVersion.ToString()
        tools = $facts
        git_python_versions_not_executed = $true
        msvc_sdk_rust_vpython_cipd_admission = 'UNBOUND'
        native_sources_complete = $false
        build_ready = $false
        distribution_ready = $false
        cache_write = $false
        build_admission_granted = $false
    } | ConvertTo-Json -Depth 5 -Compress | Write-Output
} catch {
    $message = Get-HostPreflightFailureMessage $script:HostPreflightStage $script:HostPreflightBudgetCause
    if ($Mode -ceq 'host-toolchain' -and $script:HostPreflightStage -cin
        @('role_binding', 'vs_inventory', 'sdk_inventory', 'final_identity', 'serialization')) {
        $message += Get-HostPreflightProgressMessage $script:HostPreflightWork $script:HostCount $script:HostBytes $script:HostManagedHashes
    }
    Write-Output $message
    exit 1
}
