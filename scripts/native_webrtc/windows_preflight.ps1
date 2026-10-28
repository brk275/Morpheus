param([string] $Mode = 'basic')

# Only fixed Windows system DLL exports are called. No Add-Type/compiler,
# external executable, batch file, SDK installer or shell command is invoked.
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
    $script:HostInfoMemory = [IntPtr]::Zero
    $script:HostInfoScratch = $null
    $script:HostInfoBusy = $false
    $script:HostComparisonInvoker = $null
    $script:HostComparisonType = $null
    $script:HostComparisonInfo = $null
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
    $script:HostChainInvoker = New-HostChainInvoker $script:HostComparisonInvoker
    $script:HostChainInvokerOwner = $script:HostChainInvoker
    $script:HostChainComparison = $script:HostComparisonInvoker
    # Allocate last: no further initialization can fail after unmanaged ownership
    # is acquired. The 52-byte Win32 record and ABI are unchanged.
    $script:HostInfoScratch = [byte[]]::new(52)
    $script:HostInfoMemory = [Runtime.InteropServices.Marshal]::AllocHGlobal(52)
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
        if ($PSBoundParameters.ContainsKey('Expected') -and $null -ne $script:HostComparisonInvoker -and
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
        $flags = if ($Directory) { [uint32]0x02200000 } else { [uint32]0x00200000 }
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
            $limitMembers = [Management.Automation.PSObject]::AsPSObject($limits).Members
            foreach ($key in @('Seconds', 'Entries', 'TotalBytes', 'Handles', 'MetadataBytes')) {
                if ($null -ne $limitMembers[$key]) { $managed = $false; break }
            }
            $clockMember = [Management.Automation.PSObject]::AsPSObject($script:HostWatch).Members['Elapsed']
            $secondsMember = [Management.Automation.PSObject]::AsPSObject([TimeSpan]::Zero).Members['TotalSeconds']
            $bufferMember = [Management.Automation.PSObject]::AsPSObject($buffer).Members['Length']
            $handlesMember = [Management.Automation.PSObject]::AsPSObject($script:HostHandles).Members['Count']
            $readMember = [Management.Automation.PSObject]::AsPSObject($stream).Members['Read']
            $digestMembers = [Management.Automation.PSObject]::AsPSObject($digest).Members
            $blockMember = $digestMembers['TransformBlock']; $finalMember = $digestMembers['TransformFinalBlock']
            $writeOrdinary = $null -eq $text -or
                [Management.Automation.PSObject]::AsPSObject($text).Members['Write'] -is [Management.Automation.PSMethod]
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
            # Owned scratch is freed even when closing a native file handle fails.
            $memory = $script:HostInfoMemory
            $script:HostInfoMemory = [IntPtr]::Zero
            $script:HostInfoScratch = $null
            $script:HostInfoBusy = $false
            $script:HostComparisonInvoker = $null
            $script:HostComparisonType = $null
            $script:HostComparisonInfo = $null
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
        return [ordered]@{ schema = 'morpheus-native-windows-host-toolchain-inventory-v1';
            visual_studio_root = $vs; visual_studio_year = '2022'; msvc_toolset_version = $version;
            windows_sdk_version = '10.0.26100.0'; roots = $roots; roles = $roleFacts;
            upstream_build_revision = 'be1a8f6dcd7df7e46320192c5e2f364e50d79bbf';
            upstream_vs_toolchain_sha256 = 'd13a7b17b81c2084de5a76e907c458ca997a4ddcdef96d3b74c8fcb3aca5f463';
            upstream_setup_toolchain_sha256 = 'f6ae24c99ae0bfc72b093da415615119652f25415155dffb4db22f49b78ca8cf';
            entries = $script:HostCount; total_bytes = $script:HostBytes; limits = $script:HostLimits;
            environment_realized = $false; vcvars_or_compilers_executed = $false;
            host_approval_granted = $false; build_ready = $false; distribution_ready = $false }
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
