use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::{IsInitialized, Pack},
    pubkey::Pubkey,
    sysvar::{rent::Rent, Sysvar},
};

use spl_token::state::Account as TokenAccount;

use crate::{error::EscrowError, instruction::EscrowInstruction, state::Escrow};

pub struct Processor;
impl Processor {
    pub fn process(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        instruction_data: &[u8],
    ) -> ProgramResult {
        let instruction = EscrowInstruction::unpack(instruction_data)?;

        match instruction {
            EscrowInstruction::InitEscrow { amount } => {
                msg!("Instruction: InitEscrow");
                Self::process_init_escrow(accounts, amount, program_id)
            }
            EscrowInstruction::Exchange { amount } => {
                msg!("Instruction: Exchange");
                Self::process_exchange(accounts, amount, program_id)
            }
        }
    }

    // 初始化托管
    fn process_init_escrow(
        accounts: &[AccountInfo],
        amount: u64,
        program_id: &Pubkey,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let initializer = next_account_info(account_info_iter)?;

        // 1、判断initializer是签名账号
        if !initializer.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        // 2、initializer临时账号 (已预先存入X token)
        let temp_token_account = next_account_info(account_info_iter)?;

        // 3、initializer收款账号（Y token）
        let token_to_receive_account = next_account_info(account_info_iter)?;
        if *token_to_receive_account.owner != spl_token::id() {
            return Err(ProgramError::IncorrectProgramId);
        }

        // 4、合约托管账号
        let escrow_account = next_account_info(account_info_iter)?;
        let rent = &Rent::from_account_info(next_account_info(account_info_iter)?)?;

        // 5、判断是否免租
        if !rent.is_exempt(escrow_account.lamports(), escrow_account.data_len()) {
            return Err(EscrowError::NotRentExempt.into());
        }

        // 6、反序列化托管信息结构
        let mut escrow_info = Escrow::unpack_unchecked(&escrow_account.try_borrow_data()?)?;
        if escrow_info.is_initialized() {
            return Err(ProgramError::AccountAlreadyInitialized);
        }

        // 7、托管信息赋值
        escrow_info.is_initialized = true;
        escrow_info.initializer_pubkey = *initializer.key;
        escrow_info.temp_token_account_pubkey = *temp_token_account.key;
        escrow_info.initializer_token_to_receive_account_pubkey = *token_to_receive_account.key;
        escrow_info.expected_amount = amount;

        // 8、序列化保存托管信息
        Escrow::pack(escrow_info, &mut escrow_account.try_borrow_mut_data()?)?;

        // 9、创建PDA
        let (pda, _bump_seed) = Pubkey::find_program_address(&[b"escrow"], program_id);

        // 10、将initializer临时账号 (已预先存入X token) 的权限转移给 PDA, 使 program 在exchange时有权限转出 X token 给 taker
        let token_program = next_account_info(account_info_iter)?;
        let owner_change_ix = spl_token::instruction::set_authority(
            token_program.key,
            temp_token_account.key,
            Some(&pda),
            spl_token::instruction::AuthorityType::AccountOwner,
            initializer.key,
            &[&initializer.key],
        )?;

        // 11、CPI invoke 执行权限转移
        msg!("Calling the token program to transfer token account ownership...");
        invoke(
            &owner_change_ix,
            &[
                temp_token_account.clone(),
                initializer.clone(),
                token_program.clone(),
            ],
        )?;

        Ok(())
    }

    // 交易
    fn process_exchange(
        accounts: &[AccountInfo],
        amount_expected_by_taker: u64,
        program_id: &Pubkey,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let taker = next_account_info(account_info_iter)?;

        // 1、判断taker是签名账号
        if !taker.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        // 2、taker发送 Y token 的账号
        let takers_sending_token_account = next_account_info(account_info_iter)?;

        // 3、taker接收 X token 的账号
        let takers_token_to_receive_account = next_account_info(account_info_iter)?;

        // 4、X token临时账号 (initializer已预先存入X token， 并且权限转移给了PDA)
        let pdas_temp_token_account = next_account_info(account_info_iter)?;
        let pdas_temp_token_account_info =
            TokenAccount::unpack(&pdas_temp_token_account.try_borrow_data()?)?;

        // 5、合约里再生成一次PDA和bump_seed，用于invoke_signed 执行判断
        let (pda, bump_seed) = Pubkey::find_program_address(&[b"escrow"], program_id);

        // 6、判断taker获取 X token 金额是否匹配
        if amount_expected_by_taker != pdas_temp_token_account_info.amount {
            return Err(EscrowError::ExpectedAmountMismatch.into());
        }

        // 7、initializer 主账号
        let initializers_main_account = next_account_info(account_info_iter)?;

        // 8、initializer 收取 Y token 账号
        let initializers_token_to_receive_account = next_account_info(account_info_iter)?;

        // 9、合约托管账号
        let escrow_account = next_account_info(account_info_iter)?;

        // 10、反序列化获取托管信息
        let escrow_info = Escrow::unpack(&escrow_account.try_borrow_data()?)?;

        // 11、判断参数传入的 X token 临时存储账号是否匹配
        if escrow_info.temp_token_account_pubkey != *pdas_temp_token_account.key {
            return Err(ProgramError::InvalidAccountData);
        }

        // 12、判断参数传入的 initializer 账号是否匹配
        if escrow_info.initializer_pubkey != *initializers_main_account.key {
            return Err(ProgramError::InvalidAccountData);
        }

        // 13、判断参数传入的 initializer Y token 收款账号是否匹配
        if escrow_info.initializer_token_to_receive_account_pubkey
            != *initializers_token_to_receive_account.key
        {
            return Err(ProgramError::InvalidAccountData);
        }

        let token_program = next_account_info(account_info_iter)?;

        // 14、taker的 Y token 转账给 initializer （taker签名 invoke）
        let transfer_to_initializer_ix = spl_token::instruction::transfer(
            token_program.key,
            takers_sending_token_account.key,
            initializers_token_to_receive_account.key,
            taker.key,
            &[&taker.key],
            escrow_info.expected_amount,
        )?;
        msg!("Calling the token program to transfer tokens to the escrow's initializer...");
        invoke(
            &transfer_to_initializer_ix,
            &[
                takers_sending_token_account.clone(),
                initializers_token_to_receive_account.clone(),
                taker.clone(),
                token_program.clone(),
            ],
        )?;

        let pda_account = next_account_info(account_info_iter)?;

        // 15、临时存储账号将 X token 转账给 taker （合约调用PDA签名 invoke_signed）
        let transfer_to_taker_ix = spl_token::instruction::transfer(
            token_program.key,
            pdas_temp_token_account.key,
            takers_token_to_receive_account.key,
            &pda,
            &[&pda],
            pdas_temp_token_account_info.amount,
        )?;
        msg!("Calling the token program to transfer tokens to the taker...");
        invoke_signed(
            &transfer_to_taker_ix,
            &[
                pdas_temp_token_account.clone(),
                takers_token_to_receive_account.clone(),
                pda_account.clone(),
                token_program.clone(),
            ],
            &[&[&b"escrow"[..], &[bump_seed]]],
        )?;

        // 16、关闭 X token 临时账号 （合约调用PDA签名 invoke_signed）
        let close_pdas_temp_acc_ix = spl_token::instruction::close_account(
            token_program.key,
            pdas_temp_token_account.key,
            initializers_main_account.key,
            &pda,
            &[&pda],
        )?;
        msg!("Calling the token program to close pda's temp account...");
        invoke_signed(
            &close_pdas_temp_acc_ix,
            &[
                pdas_temp_token_account.clone(),
                initializers_main_account.clone(),
                pda_account.clone(),
                token_program.clone(),
            ],
            &[&[&b"escrow"[..], &[bump_seed]]],
        )?;

        // 17、关闭托管账号，转出lamports给initializer，并清理数据
        msg!("Closing the escrow account...");
        **initializers_main_account.try_borrow_mut_lamports()? = initializers_main_account
            .lamports()
            .checked_add(escrow_account.lamports())
            .ok_or(EscrowError::AmountOverflow)?;
        **escrow_account.try_borrow_mut_lamports()? = 0;
        *escrow_account.try_borrow_mut_data()? = &mut [];

        Ok(())
    }
}
